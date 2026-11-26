//! VM-native builtins: prelude functions and the methods of `string`,
//! `List`, `Map`, `Option`, `Result` and `weak[T]`.
//!
//! String semantics: `len`, `slice`, `find`, `pad_*` work in *characters*
//! (documented); `bytes_len` exposes the UTF-8 byte length.

use std::collections::BTreeMap;
use std::rc::Rc;

use wscript_core::bytecode::Builtin;
use wscript_core::defs::{DEF_OPTION, DefId, TAG_NONE, TAG_SOME};
use wscript_core::fmt_spec::{self, FmtNum, FmtSpec};
use wscript_core::value::{Key, Value};

use crate::{RuntimeError, Vm};

fn none() -> Value {
    Value::new_enum(DEF_OPTION, TAG_NONE, vec![])
}

fn some(v: Value) -> Value {
    Value::new_enum(DEF_OPTION, TAG_SOME, vec![v])
}

impl Vm {
    pub(crate) fn call_builtin(
        &mut self,
        b: Builtin,
        args_at: usize,
        nargs: u16,
    ) -> Result<Value, RuntimeError> {
        let args: Vec<Value> = (0..nargs as usize)
            .map(|i| self.stack[args_at + i].clone())
            .collect();

        macro_rules! str_arg {
            ($i:expr) => {
                match &args[$i] {
                    Value::Str(s) => s.clone(),
                    other => {
                        return Err(self.fault(format!(
                            "type confusion: expected string, found {}",
                            other.kind_name()
                        )));
                    }
                }
            };
        }
        macro_rules! int_arg {
            ($i:expr) => {
                match &args[$i] {
                    Value::Int(n) => *n,
                    other => {
                        return Err(self.fault(format!(
                            "type confusion: expected int, found {}",
                            other.kind_name()
                        )));
                    }
                }
            };
        }
        macro_rules! list_arg {
            ($i:expr) => {
                match &args[$i] {
                    Value::List(l) => l.clone(),
                    other => {
                        return Err(self.fault(format!(
                            "type confusion: expected List, found {}",
                            other.kind_name()
                        )));
                    }
                }
            };
        }
        macro_rules! map_arg {
            ($i:expr) => {
                match &args[$i] {
                    Value::Map(m) => m.clone(),
                    other => {
                        return Err(self.fault(format!(
                            "type confusion: expected Map, found {}",
                            other.kind_name()
                        )));
                    }
                }
            };
        }
        macro_rules! enum_arg {
            ($i:expr) => {
                match &args[$i] {
                    Value::Enum(e) => e.clone(),
                    other => {
                        return Err(self.fault(format!(
                            "type confusion: expected enum, found {}",
                            other.kind_name()
                        )));
                    }
                }
            };
        }

        Ok(match b {
            // ------------------------------------------------- prelude
            Builtin::Print => {
                let s = self.display_value(&args[0])?;
                self.print_text(&s, false);
                Value::Unit
            }
            Builtin::Println => {
                let s = match args.first() {
                    Some(v) => self.display_value(v)?,
                    None => String::new(),
                };
                self.print_text(&s, true);
                Value::Unit
            }
            Builtin::Str => {
                let s = self.display_value(&args[0])?;
                Value::Str(Rc::from(s.as_str()))
            }
            Builtin::Fmt => {
                let template = str_arg!(0);
                let mut out = String::new();
                let mut arg_i = 1usize;
                let bytes = template.as_bytes();
                let mut i = 0;
                while i < bytes.len() {
                    match bytes[i] {
                        b'{' if bytes.get(i + 1) == Some(&b'{') => {
                            out.push('{');
                            i += 2;
                        }
                        b'}' if bytes.get(i + 1) == Some(&b'}') => {
                            out.push('}');
                            i += 2;
                        }
                        b'{' if bytes.get(i + 1) == Some(&b'}') => {
                            match args.get(arg_i) {
                                Some(v) => {
                                    let v = v.clone();
                                    out.push_str(&self.display_value(&v)?);
                                }
                                None => {
                                    return Err(
                                        self.fault("fmt: more `{}` placeholders than arguments")
                                    );
                                }
                            }
                            arg_i += 1;
                            i += 2;
                        }
                        // `{:spec}` — format-spec placeholder. The grammar
                        // lives in wscript_core::fmt_spec, shared with the
                        // checker's compile-time validation.
                        b'{' if bytes.get(i + 1) == Some(&b':') => {
                            let Some(end) = template[i + 2..].find('}') else {
                                return Err(self.fault("fmt: unterminated `{:` format placeholder"));
                            };
                            let spec = match fmt_spec::parse_spec(&template[i + 2..i + 2 + end]) {
                                Ok(s) => s,
                                Err(e) => return Err(self.fault(format!("fmt: {e}"))),
                            };
                            let Some(v) = args.get(arg_i).cloned() else {
                                return Err(
                                    self.fault("fmt: more `{}` placeholders than arguments")
                                );
                            };
                            let rendered = self.format_with_spec(&spec, &v)?;
                            out.push_str(&rendered);
                            arg_i += 1;
                            i += 2 + end + 1;
                        }
                        _ => {
                            let c = template[i..].chars().next().unwrap();
                            out.push(c);
                            i += c.len_utf8();
                        }
                    }
                }
                Value::Str(Rc::from(out.as_str()))
            }
            Builtin::Same => Value::Bool(args[0].same(&args[1])),
            Builtin::WeakNew => match args[0].downgrade() {
                Some(w) => Value::WeakRef(w),
                None => {
                    return Err(self.fault(format!(
                        "cannot create a weak reference to a {} value",
                        args[0].kind_name()
                    )));
                }
            },
            Builtin::WeakUpgrade => match &args[0] {
                Value::WeakRef(w) => match w.upgrade() {
                    Some(v) => some(v),
                    None => none(),
                },
                other => {
                    return Err(
                        self.fault(format!("type confusion: upgrade on {}", other.kind_name()))
                    );
                }
            },
            Builtin::IntCast => match &args[0] {
                Value::Int(n) => Value::Int(*n),
                Value::Float(f) => Value::Int(*f as i64),
                Value::Char(c) => Value::Int(*c as u32 as i64),
                other => {
                    return Err(
                        self.fault(format!("int() cannot convert from {}", other.kind_name()))
                    );
                }
            },
            Builtin::FloatCast => match &args[0] {
                Value::Int(n) => Value::Float(*n as f64),
                Value::Float(f) => Value::Float(*f),
                other => {
                    return Err(
                        self.fault(format!("float() cannot convert from {}", other.kind_name()))
                    );
                }
            },
            Builtin::FmtQuantity => {
                let Value::Int(def) = args[1] else {
                    return Err(self.fault("FmtQuantity: expected a unit family id"));
                };
                let s = self.fmt_quantity(&args[0], DefId(def as u32))?;
                Value::Str(Rc::from(s.as_str()))
            }
            Builtin::ValueEq => {
                let r = self.value_eq(&args[0], &args[1])?;
                Value::Bool(r)
            }
            Builtin::ValueCmp => {
                let r = self.value_cmp(&args[0], &args[1])?;
                Value::Int(r)
            }
            Builtin::DeepClone => self.deep_clone(&args[0])?,

            // -------------------------------------------------- string
            Builtin::StrLen => Value::Int(str_arg!(0).chars().count() as i64),
            Builtin::StrBytesLen => Value::Int(str_arg!(0).len() as i64),
            Builtin::StrIsEmpty => Value::Bool(str_arg!(0).is_empty()),
            Builtin::StrSplit => {
                let s = str_arg!(0);
                let sep = str_arg!(1);
                let parts: Vec<Value> = if sep.is_empty() {
                    s.chars()
                        .map(|c| Value::Str(Rc::from(c.to_string().as_str())))
                        .collect()
                } else {
                    s.split(&*sep).map(|p| Value::Str(Rc::from(p))).collect()
                };
                Value::new_list(parts)
            }
            Builtin::StrTrim => Value::Str(Rc::from(str_arg!(0).trim())),
            Builtin::StrTrimStart => Value::Str(Rc::from(str_arg!(0).trim_start())),
            Builtin::StrTrimEnd => Value::Str(Rc::from(str_arg!(0).trim_end())),
            Builtin::StrToUpper => Value::Str(Rc::from(str_arg!(0).to_uppercase().as_str())),
            Builtin::StrToLower => Value::Str(Rc::from(str_arg!(0).to_lowercase().as_str())),
            Builtin::StrStartsWith => Value::Bool(str_arg!(0).starts_with(&*str_arg!(1))),
            Builtin::StrEndsWith => Value::Bool(str_arg!(0).ends_with(&*str_arg!(1))),
            Builtin::StrContains => Value::Bool(str_arg!(0).contains(&*str_arg!(1))),
            Builtin::StrFind => {
                let s = str_arg!(0);
                let needle = str_arg!(1);
                match s.find(&*needle) {
                    Some(byte_idx) => {
                        let char_idx = s[..byte_idx].chars().count() as i64;
                        some(Value::Int(char_idx))
                    }
                    None => none(),
                }
            }
            Builtin::StrReplace => {
                let s = str_arg!(0);
                let from = str_arg!(1);
                let to = str_arg!(2);
                Value::Str(Rc::from(s.replace(&*from, &to).as_str()))
            }
            Builtin::StrRepeat => {
                let s = str_arg!(0);
                let n = int_arg!(1).max(0) as usize;
                if s.len().saturating_mul(n) > 64 * 1024 * 1024 {
                    return Err(self.fault("repeat: resulting string too large"));
                }
                Value::Str(Rc::from(s.repeat(n).as_str()))
            }
            Builtin::StrPadLeft | Builtin::StrPadRight => {
                let s = str_arg!(0);
                let width = int_arg!(1).max(0) as usize;
                let pad = str_arg!(2);
                let pad_char = pad.chars().next().unwrap_or(' ');
                let cur = s.chars().count();
                if cur >= width {
                    Value::Str(s)
                } else {
                    // The width is the script's number, not the size of a
                    // value it holds: it has to fit before it is allocated.
                    // The fill and the result exist together, a character
                    // is up to four bytes.
                    self.mem_reserve((width as u64).saturating_mul(8))?;
                    let fill: String = std::iter::repeat_n(pad_char, width - cur).collect();
                    let result = if b == Builtin::StrPadLeft {
                        format!("{fill}{s}")
                    } else {
                        format!("{s}{fill}")
                    };
                    Value::Str(Rc::from(result.as_str()))
                }
            }
            Builtin::StrChars => {
                let s = str_arg!(0);
                Value::new_list(s.chars().map(Value::Char).collect())
            }
            Builtin::StrSlice => {
                let s = str_arg!(0);
                let chars: Vec<char> = s.chars().collect();
                let len = chars.len() as i64;
                let start = int_arg!(1).clamp(0, len) as usize;
                let end = int_arg!(2).clamp(0, len) as usize;
                let out: String = if start < end {
                    chars[start..end].iter().collect()
                } else {
                    String::new()
                };
                Value::Str(Rc::from(out.as_str()))
            }
            Builtin::StrParseInt => {
                let s = str_arg!(0);
                match s.trim().parse::<i64>() {
                    Ok(n) => some(Value::Int(n)),
                    Err(_) => none(),
                }
            }
            Builtin::StrParseFloat => {
                let s = str_arg!(0);
                match s.trim().parse::<f64>() {
                    Ok(f) => some(Value::Float(f)),
                    Err(_) => none(),
                }
            }

            // ---------------------------------------------------- list
            Builtin::ListLen => Value::Int(list_arg!(0).borrow().len() as i64),
            Builtin::ListIsEmpty => Value::Bool(list_arg!(0).borrow().is_empty()),
            Builtin::ListPush => {
                list_arg!(0).borrow_mut().push(args[1].clone());
                Value::Unit
            }
            Builtin::ListPop => match list_arg!(0).borrow_mut().pop() {
                Some(v) => some(v),
                None => none(),
            },
            Builtin::ListGet => {
                let l = list_arg!(0);
                let i = int_arg!(1);
                let items = l.borrow();
                if i >= 0 && (i as usize) < items.len() {
                    some(items[i as usize].clone())
                } else {
                    none()
                }
            }
            Builtin::ListSet => {
                let l = list_arg!(0);
                let i = int_arg!(1);
                let mut items = l.borrow_mut();
                if i < 0 || i as usize >= items.len() {
                    let len = items.len();
                    drop(items);
                    return Err(self.fault(format!("list index {i} out of bounds (len {len})")));
                }
                items[i as usize] = args[2].clone();
                Value::Unit
            }
            Builtin::ListInsert => {
                let l = list_arg!(0);
                let i = int_arg!(1);
                let mut items = l.borrow_mut();
                if i < 0 || i as usize > items.len() {
                    let len = items.len();
                    drop(items);
                    return Err(self.fault(format!("insert index {i} out of bounds (len {len})")));
                }
                items.insert(i as usize, args[2].clone());
                Value::Unit
            }
            Builtin::ListRemove => {
                let l = list_arg!(0);
                let i = int_arg!(1);
                let mut items = l.borrow_mut();
                if i < 0 || i as usize >= items.len() {
                    let len = items.len();
                    drop(items);
                    return Err(self.fault(format!("remove index {i} out of bounds (len {len})")));
                }
                items.remove(i as usize)
            }
            Builtin::ListClear => {
                list_arg!(0).borrow_mut().clear();
                Value::Unit
            }
            Builtin::ListContains => {
                let snapshot = list_arg!(0).borrow().clone();
                let mut found = false;
                for item in &snapshot {
                    if self.value_eq(item, &args[1])? {
                        found = true;
                        break;
                    }
                }
                Value::Bool(found)
            }
            Builtin::ListIndexOf => {
                let snapshot = list_arg!(0).borrow().clone();
                let mut at = None;
                for (i, item) in snapshot.iter().enumerate() {
                    if self.value_eq(item, &args[1])? {
                        at = Some(i as i64);
                        break;
                    }
                }
                match at {
                    Some(i) => some(Value::Int(i)),
                    None => none(),
                }
            }
            Builtin::ListReverse => {
                list_arg!(0).borrow_mut().reverse();
                Value::Unit
            }
            Builtin::ListSort => {
                let l = list_arg!(0);
                // Fast path for primitive elements (homogeneous by the
                // checker); structural/Ord-impl elements go through the
                // fallible value_cmp merge sort.
                let is_prim = matches!(
                    l.borrow().first(),
                    None | Some(
                        Value::Int(_)
                            | Value::Float(_)
                            | Value::Char(_)
                            | Value::Str(_)
                            | Value::Bool(_)
                    )
                );
                if is_prim {
                    l.borrow_mut().sort_by(prim_cmp);
                } else {
                    let snapshot = l.borrow().clone();
                    let sorted = self.merge_sort(snapshot, &mut |vm, a, b| vm.value_cmp(a, b))?;
                    *l.borrow_mut() = sorted;
                }
                Value::Unit
            }
            Builtin::ListSortBy => {
                let l = list_arg!(0);
                let f = args[1].clone();
                // Snapshot semantics like map/filter: the comparator
                // re-enters script code and may mutate the receiver.
                let snapshot = l.borrow().clone();
                let sorted = self.merge_sort(snapshot, &mut |vm, a, b| match vm
                    .call_function(&f, vec![a.clone(), b.clone()])?
                {
                    Value::Int(n) => Ok(n),
                    other => Err(vm.fault(format!(
                        "sort_by: comparator returned {}, expected int (negative/0/positive)",
                        other.kind_name()
                    ))),
                })?;
                *l.borrow_mut() = sorted;
                Value::Unit
            }
            Builtin::ListAny | Builtin::ListAll => {
                let snapshot = list_arg!(0).borrow().clone();
                let f = args[1].clone();
                let mname = if matches!(b, Builtin::ListAny) {
                    "any"
                } else {
                    "all"
                };
                let mut result = matches!(b, Builtin::ListAll);
                for item in snapshot {
                    match self.call_function(&f, vec![item])? {
                        Value::Bool(hit) => {
                            if hit != result {
                                result = hit;
                                break; // short-circuit
                            }
                        }
                        other => {
                            return Err(self.fault(format!(
                                "{mname}: predicate returned {}, expected bool",
                                other.kind_name()
                            )));
                        }
                    }
                }
                Value::Bool(result)
            }
            Builtin::ListFind | Builtin::ListPosition => {
                let snapshot = list_arg!(0).borrow().clone();
                let f = args[1].clone();
                let mname = if matches!(b, Builtin::ListFind) {
                    "find"
                } else {
                    "position"
                };
                let mut hit = none();
                for (i, item) in snapshot.into_iter().enumerate() {
                    match self.call_function(&f, vec![item.clone()])? {
                        Value::Bool(true) => {
                            hit = if matches!(b, Builtin::ListFind) {
                                some(item)
                            } else {
                                some(Value::Int(i as i64))
                            };
                            break;
                        }
                        Value::Bool(false) => {}
                        other => {
                            return Err(self.fault(format!(
                                "{mname}: predicate returned {}, expected bool",
                                other.kind_name()
                            )));
                        }
                    }
                }
                hit
            }
            Builtin::ListCount => {
                let snapshot = list_arg!(0).borrow().clone();
                let f = args[1].clone();
                let mut n: i64 = 0;
                for item in snapshot {
                    match self.call_function(&f, vec![item])? {
                        Value::Bool(true) => n += 1,
                        Value::Bool(false) => {}
                        other => {
                            return Err(self.fault(format!(
                                "count: predicate returned {}, expected bool",
                                other.kind_name()
                            )));
                        }
                    }
                }
                Value::Int(n)
            }
            Builtin::ListSumInt => {
                let snapshot = list_arg!(0).borrow().clone();
                let mut acc: i64 = 0;
                for item in snapshot {
                    match item {
                        Value::Int(n) => acc = acc.wrapping_add(n),
                        other => {
                            return Err(self.fault(format!(
                                "type confusion: sum over {} elements",
                                other.kind_name()
                            )));
                        }
                    }
                }
                Value::Int(acc)
            }
            Builtin::ListSumFloat => {
                let snapshot = list_arg!(0).borrow().clone();
                let mut acc: f64 = 0.0;
                for item in snapshot {
                    match item {
                        Value::Float(f) => acc += f,
                        other => {
                            return Err(self.fault(format!(
                                "type confusion: sum over {} elements",
                                other.kind_name()
                            )));
                        }
                    }
                }
                Value::Float(acc)
            }
            Builtin::ListMin | Builtin::ListMax => {
                let snapshot = list_arg!(0).borrow().clone();
                let want_min = matches!(b, Builtin::ListMin);
                let mut best: Option<Value> = None;
                for item in snapshot {
                    best = Some(match best {
                        None => item,
                        Some(cur) => {
                            let c = self.value_cmp(&item, &cur)?;
                            if (want_min && c < 0) || (!want_min && c > 0) {
                                item
                            } else {
                                cur
                            }
                        }
                    });
                }
                match best {
                    Some(v) => some(v),
                    None => none(),
                }
            }
            Builtin::ListMapIndexed => {
                let snapshot = list_arg!(0).borrow().clone();
                let f = args[1].clone();
                let mut out = Vec::with_capacity(snapshot.len());
                for (i, item) in snapshot.into_iter().enumerate() {
                    let r = self.call_function(&f, vec![Value::Int(i as i64), item])?;
                    self.mem_hold(&r)?;
                    out.push(r);
                }
                Value::new_list(out)
            }
            Builtin::ListZipWith => {
                let a = list_arg!(0).borrow().clone();
                let bs = list_arg!(1).borrow().clone();
                let f = args[2].clone();
                let n = a.len().min(bs.len());
                let mut out = Vec::with_capacity(n);
                for i in 0..n {
                    let r = self.call_function(&f, vec![a[i].clone(), bs[i].clone()])?;
                    self.mem_hold(&r)?;
                    out.push(r);
                }
                Value::new_list(out)
            }
            Builtin::ListJoin => {
                let snapshot = list_arg!(0).borrow().clone();
                let sep = str_arg!(1);
                let mut out = String::new();
                for (i, item) in snapshot.iter().enumerate() {
                    if i > 0 {
                        out.push_str(&sep);
                    }
                    match item {
                        Value::Str(s) => out.push_str(s),
                        other => {
                            return Err(self.fault(format!(
                                "join: expected string elements, found {}",
                                other.kind_name()
                            )));
                        }
                    }
                }
                Value::Str(Rc::from(out.as_str()))
            }
            Builtin::ListMap => {
                let snapshot = list_arg!(0).borrow().clone();
                let f = args[1].clone();
                let mut out = Vec::with_capacity(snapshot.len());
                for item in snapshot {
                    let r = self.call_function(&f, vec![item])?;
                    self.mem_hold(&r)?;
                    out.push(r);
                }
                Value::new_list(out)
            }
            Builtin::ListFilter => {
                let snapshot = list_arg!(0).borrow().clone();
                let f = args[1].clone();
                let mut out = Vec::new();
                for item in snapshot {
                    match self.call_function(&f, vec![item.clone()])? {
                        Value::Bool(true) => out.push(item),
                        Value::Bool(false) => {}
                        other => {
                            return Err(self.fault(format!(
                                "filter: predicate returned {}, expected bool",
                                other.kind_name()
                            )));
                        }
                    }
                }
                Value::new_list(out)
            }
            Builtin::ListFold => {
                let snapshot = list_arg!(0).borrow().clone();
                let mut acc = args[1].clone();
                let f = args[2].clone();
                for item in snapshot {
                    acc = self.call_function(&f, vec![acc, item])?;
                }
                acc
            }
            Builtin::ListFirst => match list_arg!(0).borrow().first() {
                Some(v) => some(v.clone()),
                None => none(),
            },
            Builtin::ListLast => match list_arg!(0).borrow().last() {
                Some(v) => some(v.clone()),
                None => none(),
            },
            Builtin::ListSlice => {
                let l = list_arg!(0);
                let items = l.borrow();
                let len = items.len() as i64;
                let start = int_arg!(1).clamp(0, len) as usize;
                let end = int_arg!(2).clamp(0, len) as usize;
                let out: Vec<Value> = if start < end {
                    items[start..end].to_vec()
                } else {
                    Vec::new()
                };
                drop(items);
                Value::new_list(out)
            }
            Builtin::ListConcat => {
                let a = list_arg!(0).borrow().clone();
                let b_items = list_arg!(1).borrow().clone();
                let mut out = a;
                out.extend(b_items);
                Value::new_list(out)
            }
            Builtin::ListClone => {
                let v = args[0].clone();
                self.deep_clone(&v)?
            }

            // ----------------------------------------------------- map
            Builtin::MapLen => Value::Int(map_arg!(0).borrow().len() as i64),
            Builtin::MapIsEmpty => Value::Bool(map_arg!(0).borrow().is_empty()),
            Builtin::MapInsert => {
                let key = self.as_key(&args[1])?;
                map_arg!(0).borrow_mut().insert(key, args[2].clone());
                Value::Unit
            }
            Builtin::MapRemove => {
                let key = self.as_key(&args[1])?;
                match map_arg!(0).borrow_mut().remove(&key) {
                    Some(v) => some(v),
                    None => none(),
                }
            }
            Builtin::MapGet => {
                let key = self.as_key(&args[1])?;
                match map_arg!(0).borrow().get(&key) {
                    Some(v) => some(v.clone()),
                    None => none(),
                }
            }
            Builtin::MapContainsKey => {
                let key = self.as_key(&args[1])?;
                Value::Bool(map_arg!(0).borrow().contains_key(&key))
            }
            Builtin::MapKeys => {
                let m = map_arg!(0);
                let keys: Vec<Value> = m.borrow().keys().map(|k| k.to_value()).collect();
                Value::new_list(keys)
            }
            Builtin::MapValues => {
                let m = map_arg!(0);
                let values: Vec<Value> = m.borrow().values().cloned().collect();
                Value::new_list(values)
            }
            Builtin::MapClear => {
                map_arg!(0).borrow_mut().clear();
                Value::Unit
            }
            Builtin::MapClone => {
                let m = map_arg!(0);
                let snapshot = m.borrow().clone();
                let mut out = BTreeMap::new();
                for (k, x) in snapshot {
                    out.insert(k, self.deep_clone(&x)?);
                }
                Value::new_map(out)
            }
            Builtin::MapEach => {
                let snapshot = map_arg!(0).borrow().clone();
                let f = args[1].clone();
                for (k, v) in snapshot {
                    self.call_function(&f, vec![k.to_value(), v])?;
                }
                Value::Unit
            }
            Builtin::MapMapEntries => {
                let snapshot = map_arg!(0).borrow().clone();
                let f = args[1].clone();
                let mut out = Vec::with_capacity(snapshot.len());
                for (k, v) in snapshot {
                    let r = self.call_function(&f, vec![k.to_value(), v])?;
                    self.mem_hold(&r)?;
                    out.push(r);
                }
                Value::new_list(out)
            }
            Builtin::MapFilter => {
                let snapshot = map_arg!(0).borrow().clone();
                let f = args[1].clone();
                let mut out = BTreeMap::new();
                for (k, v) in snapshot {
                    match self.call_function(&f, vec![k.to_value(), v.clone()])? {
                        Value::Bool(true) => {
                            out.insert(k, v);
                        }
                        Value::Bool(false) => {}
                        other => {
                            return Err(self.fault(format!(
                                "filter: predicate returned {}, expected bool",
                                other.kind_name()
                            )));
                        }
                    }
                }
                Value::new_map(out)
            }

            // ------------------------------------------ option / result
            Builtin::OptionIsSome => Value::Bool(enum_arg!(0).tag == TAG_SOME),
            Builtin::OptionIsNone => Value::Bool(enum_arg!(0).tag == TAG_NONE),
            Builtin::OptionUnwrap => {
                let e = enum_arg!(0);
                if e.tag == TAG_SOME {
                    e.fields.borrow()[0].clone()
                } else {
                    return Err(self.fault("called unwrap() on None"));
                }
            }
            Builtin::OptionUnwrapOr => {
                let e = enum_arg!(0);
                if e.tag == TAG_SOME {
                    e.fields.borrow()[0].clone()
                } else {
                    args[1].clone()
                }
            }
            Builtin::OptionExpect => {
                let e = enum_arg!(0);
                if e.tag == TAG_SOME {
                    e.fields.borrow()[0].clone()
                } else {
                    let msg = str_arg!(1);
                    return Err(self.fault(msg.to_string()));
                }
            }
            Builtin::ResultIsOk => Value::Bool(enum_arg!(0).tag == wscript_core::defs::TAG_OK),
            Builtin::ResultIsErr => Value::Bool(enum_arg!(0).tag == wscript_core::defs::TAG_ERR),
            Builtin::ResultUnwrap => {
                let e = enum_arg!(0);
                if e.tag == wscript_core::defs::TAG_OK {
                    e.fields.borrow()[0].clone()
                } else {
                    let err = e.fields.borrow()[0].clone();
                    let shown = self.display_value(&err)?;
                    return Err(self.fault(format!("called unwrap() on Err: {shown}")));
                }
            }
            Builtin::ResultUnwrapOr => {
                let e = enum_arg!(0);
                if e.tag == wscript_core::defs::TAG_OK {
                    e.fields.borrow()[0].clone()
                } else {
                    args[1].clone()
                }
            }
            Builtin::ResultUnwrapErr => {
                let e = enum_arg!(0);
                if e.tag == wscript_core::defs::TAG_ERR {
                    e.fields.borrow()[0].clone()
                } else {
                    return Err(self.fault("called unwrap_err() on Ok"));
                }
            }
            Builtin::ResultExpect => {
                let e = enum_arg!(0);
                if e.tag == wscript_core::defs::TAG_OK {
                    e.fields.borrow()[0].clone()
                } else {
                    // `<msg>: <err>`, as Rust's `Result::expect` reads: the
                    // message names the step, the `Err` payload says why.
                    let msg = str_arg!(1).to_string();
                    let err = e.fields.borrow()[0].clone();
                    let shown = self.display_value(&err)?;
                    return Err(self.fault(format!("{msg}: {shown}")));
                }
            }
        })
    }

    fn as_key(&self, v: &Value) -> Result<Key, RuntimeError> {
        Key::from_value(v)
            .ok_or_else(|| self.fault(format!("invalid map key of type {}", v.kind_name())))
    }

    /// Stable merge sort threading a fallible, script-re-entering
    /// comparator (`Vec::sort_by` can neither fail nor take `&mut Vm`).
    /// Operates on a snapshot; callers write the result back. Ties keep
    /// the left/earlier element (stability).
    fn merge_sort(
        &mut self,
        items: Vec<Value>,
        cmp: &mut impl FnMut(&mut Vm, &Value, &Value) -> Result<i64, RuntimeError>,
    ) -> Result<Vec<Value>, RuntimeError> {
        if items.len() < 2 {
            return Ok(items);
        }
        let mid = items.len() / 2;
        let mut right = items;
        let left = self.merge_sort(right.drain(..mid).collect(), cmp)?;
        let right = self.merge_sort(right, cmp)?;
        let mut out = Vec::with_capacity(left.len() + right.len());
        let (mut li, mut ri) = (0, 0);
        while li < left.len() && ri < right.len() {
            if cmp(self, &right[ri], &left[li])? < 0 {
                out.push(right[ri].clone());
                ri += 1;
            } else {
                out.push(left[li].clone());
                li += 1;
            }
        }
        out.extend_from_slice(&left[li..]);
        out.extend_from_slice(&right[ri..]);
        Ok(out)
    }

    /// Render one value under a `{:spec}` format spec. Base conversion and
    /// precision happen here (they need the typed value); padding is the
    /// shared `fmt_spec::pad_spec`.
    fn format_with_spec(&mut self, spec: &FmtSpec, v: &Value) -> Result<String, RuntimeError> {
        let plain = if let Some(num) = spec.num {
            let Value::Int(n) = v else {
                return Err(self.fault(format!(
                    "fmt: `{{:x}}`-style base formats require an int, found {}",
                    v.kind_name()
                )));
            };
            match num {
                FmtNum::HexLower => format!("{n:x}"),
                FmtNum::HexUpper => format!("{n:X}"),
                FmtNum::Binary => format!("{n:b}"),
                FmtNum::Octal => format!("{n:o}"),
            }
        } else if let Some(p) = spec.prec {
            match v {
                Value::Float(f) => format!("{f:.p$}"),
                Value::Str(_) => self.display_value(v)?.chars().take(p).collect(),
                other => {
                    return Err(self.fault(format!(
                        "fmt: a `.{p}` precision requires a float or string, found {}",
                        other.kind_name()
                    )));
                }
            }
        } else {
            self.display_value(v)?
        };
        let numeric = matches!(v, Value::Int(_) | Value::Float(_));
        Ok(fmt_spec::pad_spec(spec, plain, numeric))
    }
}

/// Total order over primitive values (list sort — checker restricts
/// elements to int/float/char/string).
fn prim_cmp(a: &Value, b: &Value) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => x.cmp(y),
        (Value::Float(x), Value::Float(y)) => x.total_cmp(y),
        (Value::Char(x), Value::Char(y)) => x.cmp(y),
        (Value::Str(x), Value::Str(y)) => x.cmp(y),
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        _ => Ordering::Equal,
    }
}
