// vmlab-agent for TempleOS (PRD §7.4, the legacy tier): the same wire as
// guest/agent-proto, version 2, over the 16550 at COM1 that an
// `agent_transport = "isa-serial"` profile wires to the host socket.
//
// One feature, `exec`: a command is HolyC source, compiled and run by the
// OS's own compiler. Output is captured by marking the task document's tail
// before the run and reading back the entries the run appended — printing
// goes into that document, and there is no redirection hook to borrow. An
// exception, including a compile error, becomes exit code 1 with the OS's
// own report in the output. Everything else the wire can ask for is refused
// by name, so the host's feature ladder degrades truthfully.
//
// Ring 0, direct port I/O, no interrupts: the UART is polled from one task
// spawned at boot by ~/MakeHome.HC. A command runs synchronously in that
// task, so the agent answers nothing else until it returns — the host sees
// latency, never a lost agent.

#define VA_BASE     0x3F8
#define VA_MAX_PAY  65536
#define VA_WINDOW   262144
#define VA_CHUNK    4096
#define VA_MAX_TOK  256
#define VA_VERSION  "agent-templeos=dev"

// ---- UART ------------------------------------------------------------

U0 VaUartInit()
{
  OutU8(VA_BASE+1,0x00);     // no interrupts
  OutU8(VA_BASE+3,0x80);     // divisor latch
  OutU8(VA_BASE+0,0x01);     // 115200
  OutU8(VA_BASE+1,0x00);
  OutU8(VA_BASE+3,0x03);     // 8N1
  OutU8(VA_BASE+2,0xC7);     // FIFO on, cleared
  OutU8(VA_BASE+4,0x0B);     // DTR | RTS | OUT2
}

U8 VaRx()
{
  while (!(InU8(VA_BASE+5)&1))
    Yield;
  return InU8(VA_BASE);
}

U0 VaTx(U8 c)
{
  while (!(InU8(VA_BASE+5)&0x20));
  OutU8(VA_BASE,c);
}

U0 VaTxBuf(U8 *p,I64 n)
{
  while (n-->0)
    VaTx(*p++);
}

// ---- frames ----------------------------------------------------------

U8 *va_pay;          // one payload, VA_MAX_PAY
I64 va_kind,va_chan,va_len;
U8 *va_magic="VMLB";

// Read one frame into va_pay. Blocks until a whole frame has arrived;
// anything before the magic is skipped.
U0 VaReadFrame()
{
  I64 m=0,i;
  U8 c;
  while (m<4) {
    c=VaRx;
    if (c==va_magic[m]) m++;
    else if (c=='V') m=1;
    else m=0;
  }
  va_len=0;
  for (i=0;i<4;i++) va_len|=VaRx<<(8*i);
  va_kind=VaRx;
  va_chan=0;
  for (i=0;i<4;i++) va_chan|=VaRx<<(8*i);
  if (va_len>VA_MAX_PAY) { va_len=0; va_kind=-1; return; }
  for (i=0;i<va_len;i++) va_pay[i]=VaRx;
  va_pay[va_len]=0;
}

U0 VaSendFrame(I64 kind,I64 chan,U8 *p,I64 n)
{
  U8 h[13];
  I64 i;
  MemCpy(h,"VMLB",4);
  for (i=0;i<4;i++) h[4+i]=(n>>(8*i))&0xFF;
  h[8]=kind;
  for (i=0;i<4;i++) h[9+i]=(chan>>(8*i))&0xFF;
  VaTxBuf(h,13);
  if (n) VaTxBuf(p,n);
}

// ---- JSON ------------------------------------------------------------

#define JT_UNDEF 0
#define JT_OBJ   1
#define JT_ARR   2
#define JT_STR   3
#define JT_PRIM  4

class CVaTok { I64 type,start,end,size,parent; };

CVaTok va_tok[VA_MAX_TOK];
I64 va_ntok,va_pos,va_parent;
U8 *va_js;
I64 va_jslen;

I64 VaAlloc(I64 type,I64 start)
{
  CVaTok *t;
  if (va_ntok>=VA_MAX_TOK) return -1;
  t=&va_tok[va_ntok];
  t->type=type; t->start=start; t->end=-1; t->size=0; t->parent=va_parent;
  if (va_parent>=0) va_tok[va_parent].size++;
  return va_ntok++;
}

Bool VaWs(U8 c) { return c==' '||c=='\t'||c=='\n'||c=='\r'; }

I64 VaParseStr()
{
  I64 start=va_pos,i;
  U8 c;
  va_pos++;
  while (va_pos<va_jslen) {
    c=va_js[va_pos];
    if (c=='"') {
      i=VaAlloc(JT_STR,start+1);
      if (i<0) return -1;
      va_tok[i].end=va_pos;
      va_pos++;
      return 0;
    }
    if (c=='\\') {
      va_pos++;
      if (va_pos>=va_jslen) return -1;
      if (va_js[va_pos]=='u') va_pos+=4;
    }
    va_pos++;
  }
  return -1;
}

I64 VaParsePrim()
{
  I64 start=va_pos,i;
  U8 c;
  while (va_pos<va_jslen) {
    c=va_js[va_pos];
    if (VaWs(c)||c==','||c==']'||c=='}') break;
    va_pos++;
  }
  i=VaAlloc(JT_PRIM,start);
  if (i<0) return -1;
  va_tok[i].end=va_pos;
  return 0;
}

I64 VaParseVal()
{
  I64 me,saved,is_obj,key;
  while (va_pos<va_jslen && VaWs(va_js[va_pos])) va_pos++;
  if (va_pos>=va_jslen) return -1;
  switch (va_js[va_pos]) {
    case '"':
      return VaParseStr;
    case '{':
    case '[':
      is_obj=va_js[va_pos]=='{';
      if (is_obj) me=VaAlloc(JT_OBJ,va_pos);
      else me=VaAlloc(JT_ARR,va_pos);
      saved=va_parent;
      if (me<0) return -1;
      va_pos++;
      va_parent=me;
      while (TRUE) {
        while (va_pos<va_jslen && VaWs(va_js[va_pos])) va_pos++;
        if (va_pos>=va_jslen) return -1;
        if ((is_obj && va_js[va_pos]=='}') || (!is_obj && va_js[va_pos]==']')) {
          va_pos++;
          break;
        }
        if (va_tok[me].size>0) {
          if (va_js[va_pos]!=',') return -1;
          va_pos++;
          while (va_pos<va_jslen && VaWs(va_js[va_pos])) va_pos++;
        }
        if (is_obj) {
          if (va_pos>=va_jslen || va_js[va_pos]!='"') return -1;
          if (VaParseStr<0) return -1;
          key=va_ntok-1;
          while (va_pos<va_jslen && VaWs(va_js[va_pos])) va_pos++;
          if (va_pos>=va_jslen || va_js[va_pos]!=':') return -1;
          va_pos++;
          va_parent=key;
          if (VaParseVal<0) return -1;
          va_parent=me;
        } else {
          if (VaParseVal<0) return -1;
        }
      }
      va_tok[me].end=va_pos;
      va_parent=saved;
      return 0;
    default:
      return VaParsePrim;
  }
}

I64 VaJsonParse(U8 *js,I64 len)
{
  va_js=js; va_jslen=len; va_pos=0; va_ntok=0; va_parent=-1;
  if (VaParseVal<0) return -1;
  return va_ntok;
}

I64 VaSkip(I64 i)
{
  I64 end;
  if (i<0 || i>=va_ntok) return va_ntok;
  end=va_tok[i].end;
  i++;
  while (i<va_ntok && va_tok[i].start<end) i++;
  return i;
}

Bool VaStrEq(I64 i,U8 *s)
{
  I64 n=StrLen(s);
  if (va_tok[i].type!=JT_STR) return FALSE;
  return va_tok[i].end-va_tok[i].start==n && !MemCmp(va_js+va_tok[i].start,s,n);
}

I64 VaGet(I64 obj,U8 *key)
{
  I64 i;
  if (obj<0 || obj>=va_ntok || va_tok[obj].type!=JT_OBJ) return -1;
  i=obj+1;
  while (i<va_ntok && va_tok[i].parent==obj) {
    if (VaStrEq(i,key) && i+1<va_ntok) return i+1;
    i=VaSkip(i+1);
  }
  return -1;
}

// Decode string token i into out (cap bytes). Non-ASCII escapes become '?':
// TempleOS has no Unicode. Returns the length, -1 when it does not fit.
I64 VaStr(I64 i,U8 *out,I64 cap)
{
  I64 p,n=0;
  U8 c;
  if (va_tok[i].type!=JT_STR) { out[0]=0; return -1; }
  for (p=va_tok[i].start;p<va_tok[i].end;p++) {
    c=va_js[p];
    if (c=='\\') {
      p++;
      c=va_js[p];
      if (c=='n') c='\n';
      else if (c=='t') c='\t';
      else if (c=='r') c='\r';
      else if (c=='u') { p+=4; c='?'; }
    }
    if (n+1>=cap) { out[cap-1]=0; return -1; }
    out[n++]=c;
  }
  out[n]=0;
  return n;
}

I64 VaNum(I64 i,I64 dflt)
{
  I64 p,v=0;
  if (i<0 || va_tok[i].type!=JT_PRIM) return dflt;
  for (p=va_tok[i].start;p<va_tok[i].end;p++) {
    if (va_js[p]<'0'||va_js[p]>'9') return dflt;
    v=v*10+va_js[p]-'0';
  }
  return v;
}

// ---- control messages ------------------------------------------------

U8 *va_out;    // one control message under construction
I64 va_outlen;

U0 VaOutRaw(U8 *s)
{
  I64 n=StrLen(s);
  MemCpy(va_out+va_outlen,s,n);
  va_outlen+=n;
  va_out[va_outlen]=0;
}

U0 VaOutStr(U8 *s)
{
  U8 tmp[8],c;
  va_out[va_outlen++]='"';
  for (;*s;s++) {
    c=*s;
    if (c=='"') VaOutRaw("\\\"");
    else if (c=='\\') VaOutRaw("\\\\");
    else if (c=='\n') VaOutRaw("\\n");
    else if (c=='\r') VaOutRaw("\\r");
    else if (c=='\t') VaOutRaw("\\t");
    else if (c<0x20) { StrPrint(tmp,"\\u%04X",c); VaOutRaw(tmp); }
    else if (c>=0x80) VaOutRaw("?");
    else va_out[va_outlen++]=c;
  }
  va_out[va_outlen++]='"';
  va_out[va_outlen]=0;
}

U0 VaOutNum(I64 v)
{
  U8 tmp[24];
  StrPrint(tmp,"%d",v);
  VaOutRaw(tmp);
}

U0 VaBegin(U8 *event)
{
  va_outlen=0;
  VaOutRaw("{\"event\":");
  VaOutStr(event);
}

U0 VaEnd()
{
  VaOutRaw("}");
  VaSendFrame(0,0,va_out,va_outlen);
}

U0 VaSendSimple(U8 *event,I64 id)
{
  VaBegin(event);
  if (id>=0) { VaOutRaw(",\"id\":"); VaOutNum(id); }
  VaEnd;
}

U0 VaSendError(I64 id,U8 *msg)
{
  VaBegin("error");
  if (id>=0) { VaOutRaw(",\"id\":"); VaOutNum(id); }
  VaOutRaw(",\"msg\":");
  VaOutStr(msg);
  VaEnd;
}

U0 VaSendHello(U8 *token)
{
  VaBegin("hello");
  VaOutRaw(",\"proto_version\":2,\"agent_version\":");
  VaOutStr(VA_VERSION);
  VaOutRaw(",\"os\":\"templeos\",\"features\":[\"exec\"],\"token\":");
  VaOutStr(token);
  VaEnd;
}

U0 VaSendOsInfo()
{
  U8 ver[32];
  StrPrint(ver,"%5.2f",sys_os_version);
  VaBegin("os_info");
  VaOutRaw(",\"info\":{\"id\":\"templeos\",\"name\":\"TempleOS\",\"version\":");
  VaOutStr(ver);
  VaOutRaw(",\"kernel\":");
  VaOutStr(ver);
  VaOutRaw(",\"arch\":\"x86_64\",\"hostname\":\"templeos\"}");
  VaEnd;
}

// ---- exec ------------------------------------------------------------

I64 va_credit;   // guest->host credit on the one live channel

// Serve control frames until the host grants credit on chan; data frames
// (stdin) are acknowledged and discarded — a HolyC expression reads no
// stdin.
U0 VaWaitCredit(I64 chan)
{
  I64 t,cmd;
  while (va_credit<=0) {
    VaReadFrame;
    // HolyC has no `continue`: a malformed message just falls through.
    if (va_kind==0 && va_chan==0 && VaJsonParse(va_pay,va_len)>=0) {
      cmd=VaGet(0,"cmd");
      if (cmd>=0 && VaStrEq(cmd,"window_adjust") && VaNum(VaGet(0,"id"),-1)==chan)
        va_credit+=VaNum(VaGet(0,"bytes"),0);
      else if (cmd>=0 && VaStrEq(cmd,"ping"))
        VaSendSimple("pong",-1);
      else if (cmd>=0 && VaStrEq(cmd,"close") && VaNum(VaGet(0,"id"),-1)==chan)
        { va_credit=-1; return; }
    }
  }
}

// Send captured text as DATA frames under the credit window.
U0 VaSendText(I64 chan,U8 *buf,I64 len)
{
  I64 off=0,n;
  va_credit=VA_WINDOW;
  while (off<len) {
    n=len-off;
    if (n>VA_CHUNK) n=VA_CHUNK;
    VaWaitCredit(chan);
    if (va_credit<0) return;
    if (n>va_credit) n=va_credit;
    VaSendFrame(1,chan,buf+off,n);
    va_credit-=n;
    off+=n;
  }
}

// Read back what a run appended to the task's document: entries are text
// (their `tag`) and newlines, from the entry after `mark` to the end.
I64 VaCapture(CDoc *d,CDocEntry *mark,U8 *out,I64 cap)
{
  CDocEntry *e;
  I64 n=0,l;
  out[0]=0;
  if (!d) return 0;
  // A document that has never been printed to has no tail, so the mark is
  // NULL and everything in it is new.
  if (mark) e=mark->next;
  else e=d->head.next;
  while (e && e!=&d->head && n<cap-1) {
    if (e->type_u8==DOCT_NEW_LINE) {
      out[n++]='\n';
    } else if (e->type_u8==DOCT_TAB) {
      out[n++]='\t';
    } else if (e->tag) {
      l=StrLen(e->tag);
      if (n+l>=cap-1) l=cap-1-n;
      MemCpy(out+n,e->tag,l);
      n+=l;
    }
    e=e->next;
  }
  out[n]=0;
  return n;
}

U0 VaExec(I64 id)
{
  I64 argv_tok,t,i,n=0,l;
  U8 *src=MAlloc(VA_MAX_PAY);
  U8 *arg=MAlloc(VA_MAX_PAY);
  U8 *out;
  CDoc *cap,*old_put,*old_disp;
  I64 code=0,outlen;

  if (VaGet(0,"logon")>=0 && va_tok[VaGet(0,"logon")].type==JT_OBJ) {
    VaSendError(id,"templeos agent: no logon; TempleOS has one identity");
    Free(src); Free(arg); return;
  }
  argv_tok=VaGet(0,"argv");
  if (argv_tok<0 || va_tok[argv_tok].type!=JT_ARR || va_tok[argv_tok].size==0) {
    VaSendError(id,"exec: empty argv");
    Free(src); Free(arg); return;
  }
  // The argv joined by spaces is the HolyC to compile.
  t=argv_tok+1;
  for (i=0;i<va_tok[argv_tok].size;i++) {
    if (VaStr(t,arg,VA_MAX_PAY)<0) {
      VaSendError(id,"exec: argument too long");
      Free(src); Free(arg); return;
    }
    l=StrLen(arg);
    if (n+l+2>=VA_MAX_PAY) {
      VaSendError(id,"exec: command too long");
      Free(src); Free(arg); return;
    }
    if (i) src[n++]=' ';
    MemCpy(src+n,arg,l);
    n+=l;
    t=VaSkip(t);
  }
  src[n]=0;
  if (n && src[n-1]!=';' && src[n-1]!='}') { src[n++]=';'; src[n]=0; }
  VaSendSimple("opened",id);

  // Run with a document of our own in place, then read it back. Printing
  // follows the task's `display_doc`; `put_doc` is its double-buffer pair
  // and both are swapped so neither route escapes to the guest's screen.
  cap=DocNew;
  old_put=Fs->put_doc;
  old_disp=Fs->display_doc;
  Fs->put_doc=cap;
  Fs->display_doc=cap;
  try {
    ExePrint("%s",src);
  } catch {
    Fs->catch_except=TRUE;
    PutExcept;
    code=1;
  }
  Fs->put_doc=old_put;
  Fs->display_doc=old_disp;
  out=MAlloc(VA_MAX_PAY);
  outlen=VaCapture(cap,NULL,out,VA_MAX_PAY);
  DocDel(cap);
  // A compile error is reported by the compiler rather than thrown past
  // ExePrint, so the captured text is the only signal there is.
  if (StrFind("ERROR:",out)) code=1;
  VaSendText(id,out,outlen);
  Free(out);
  if (va_credit>=0) {
    VaSendSimple("eof",id);
    VaBegin("exited");
    VaOutRaw(",\"id\":"); VaOutNum(id);
    VaOutRaw(",\"code\":"); VaOutNum(code);
    VaEnd;
  }
  Free(src); Free(arg);
}

// ---- dispatch --------------------------------------------------------

U0 VaHandleCtrl()
{
  I64 cmd,id,t;
  U8 name[64],token[256],mode[32];
  if (VaJsonParse(va_pay,va_len)<0 || va_tok[0].type!=JT_OBJ) {
    VaSendError(-1,"unparseable control message");
    return;
  }
  cmd=VaGet(0,"cmd");
  if (cmd<0 || VaStr(cmd,name,64)<0) {
    VaSendError(-1,"control message without a cmd");
    return;
  }
  id=VaNum(VaGet(0,"id"),0);
  if (!StrCmp(name,"hello")) {
    t=VaGet(0,"token");
    if (t<0 || VaStr(t,token,256)<0) token[0]=0;
    VaSendHello(token);
  } else if (!StrCmp(name,"open_exec")) {
    VaExec(id);
  } else if (!StrCmp(name,"ping")) {
    VaSendSimple("pong",-1);
  } else if (!StrCmp(name,"os_info")) {
    VaSendOsInfo;
  } else if (!StrCmp(name,"net_info")) {
    VaBegin("net_info");
    VaOutRaw(",\"interfaces\":[]");
    VaEnd;
  } else if (!StrCmp(name,"shutdown")) {
    t=VaGet(0,"mode");
    if (t<0 || VaStr(t,mode,32)<0) StrCpy(mode,"powerdown");
    VaBegin("shutting_down");
    VaOutRaw(",\"mode\":");
    VaOutStr(mode);
    VaEnd;
    Sleep(200);
    if (!StrCmp(mode,"reboot")) Reboot;
    // PIIX4 PM1a control: SLP_EN with the S5 type QEMU's i440fx uses.
    OutU16(0x604,0x2000);
    Sleep(2000);
    VaSendError(-1,"shutdown: power-off did not take");
  } else if (!StrCmp(name,"eof") || !StrCmp(name,"close") ||
             !StrCmp(name,"window_adjust") || !StrCmp(name,"resize")) {
    // No channel is open outside VaExec, which handles its own.
  } else if (!StrCmp(name,"open_terminal")) {
    VaSendError(id,"templeos agent: no terminal (features: exec)");
  } else if (!StrCmp(name,"open_fileops")) {
    VaSendError(id,"templeos agent: no fileops (features: exec)");
  } else if (!StrCmp(name,"open_tunnel")) {
    VaSendError(id,"templeos agent: no tunnel (features: exec)");
  } else if (!StrCmp(name,"open_tail")) {
    VaSendError(id,"templeos agent: no tail (features: exec)");
  } else if (!StrCmp(name,"open_watch")) {
    VaSendError(id,"templeos agent: no watch (features: exec)");
  } else if (!StrCmp(name,"open_eventlog")) {
    VaSendError(id,"templeos agent: no eventlog (features: exec)");
  } else {
    VaSendError(-1,"templeos agent: not available (features: exec)");
  }
}

U0 VmlabAgentTask(U8 *data)
{
  no_warn data;
  va_pay=MAlloc(VA_MAX_PAY+1);
  va_out=MAlloc(VA_MAX_PAY);
  VaUartInit;
  while (TRUE) {
    VaReadFrame;
    if (va_kind==0 && va_chan==0)
      VaHandleCtrl;
    // DATA frames outside an exec belong to nothing: dropped.
  }
}

U0 VmlabAgentStart()
{
  Spawn(&VmlabAgentTask,NULL,"VmlabAgent");
}

// Register the agent to start at every boot — ~/MakeHome.HC is what
// StartOS.HC includes last — and start it now. Idempotent: a rebuild that
// already registered it changes nothing.
U0 VmlabAgentInstall()
{
  I64 sz;
  U8 *old=FileRead("~/MakeHome.HC",&sz),*nw;
  if (!old) { old=StrNew(""); sz=0; }
  if (!StrFind("VmlabAgt",old)) {
    nw=MAlloc(sz+128);
    MemCpy(nw,old,sz);
    nw[sz]=0;
    CatPrint(nw,"\n#include \"~/VmlabAgt\"\nVmlabAgentStart;\n");
    // The uncompressed name: `MakeHome.HC.Z` is compressed on disk, and
    // plain text written under it would not read back.
    FileWrite("~/MakeHome.HC",nw,StrLen(nw));
    Free(nw);
  }
  Free(old);
  VmlabAgentStart;
  "vmlab agent installed and started on COM1\n";
}
