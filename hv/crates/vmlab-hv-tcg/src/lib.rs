//! Reference execution of the same specialised IR consumed by the trace compiler.
mod continuation;
mod cpu;
mod decode;
mod globals;
mod indexed;
mod intrinsics;
mod memo;
mod publication;
mod record;
mod trace;
pub use globals::Globals;
#[cfg(feature = "lockstep")]
pub mod lockstep;
mod memory;
mod softfloat;
mod value;
pub use cpu::{Privilege, Riscv64, SpecializationStats, TraceProfile};
pub use memory::{CodeStamp, FlatMemory, Memory, RESERVATION_GRANULE, SharedMemory};
use num_bigint::BigUint;
use std::collections::BTreeMap;
pub use value::Value;
use vmlab_hv_tcg_ir::{Expr, Function, Location, Op, Program, Target, Type};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("type error: {0}")]
    Type(String),
    #[error("undefined value: {0}")]
    Undefined(String),
    #[error("unsupported intrinsic: {0}")]
    Intrinsic(String),
    #[error("Sail exception: {0}")]
    Exception(String),
    #[error("physical access fault at {0:#x}")]
    Memory(u64),
    #[error("execution limit exceeded")]
    Limit,
    #[error("{function} IR instruction {instruction}: {source}")]
    At {
        function: String,
        instruction: usize,
        source: Box<Error>,
    },
}

impl Error {
    /// The address of the physical access the host memory refused, when that
    /// is what this error is.
    pub(crate) fn host_memory_fault(&self) -> Option<u64> {
        match self {
            Self::Memory(address) => Some(*address),
            Self::At { source, .. } => source.host_memory_fault(),
            _ => None,
        }
    }
}

/// The two-field struct `(physical address, ExceptionType)` in the `Err` of a
/// checked access. The field names are this prefix and the field number.
const ACCESS_ERROR_TUPLE: &str = "ztuplez3z5bv_z5unionz0zzExceptionType";

/// The `Err` constructor of the model function that performs a physical
/// access after its PMP and PMA checks, for the two functions that do.
fn checked_access_error(function: &str) -> Option<&'static str> {
    match function {
        "zchecked_mem_read" => Some("zErrzIz8bzCuz9zCz8bzCUExceptionTypez9zK"),
        "zchecked_mem_write" => Some("zErrzIozCz8bzCUExceptionTypez9zK"),
        _ => None,
    }
}

pub(crate) fn is_checked_access(function: &str) -> bool {
    checked_access_error(function).is_some()
}

/// The `MemoryAccessType` in a model `Mem_request`: its `access_kind` is
/// `AK_Read` or `AK_Write` of a `(kind, access type)` pair, and the kind is
/// an enum, so the access type is the pair's one union.
fn requested_access(request: &Value) -> Option<Value> {
    let Value::Struct(request) = request else {
        return None;
    };
    let Value::Union(_, pair) = request.get("zaccess_kind")? else {
        return None;
    };
    let Value::Struct(pair) = pair.as_ref() else {
        return None;
    };
    pair.values()
        .find(|value| matches!(value, Value::Union(..)))
        .cloned()
}

type Frame = BTreeMap<String, Value>;

/// The IR operations one guest instruction may take before `Error::Limit`.
pub const STEP_OPERATION_BUDGET: u64 = 100_000_000;

/// The most guest console output an interpreter keeps. A guest can print
/// without end, so the oldest bytes are dropped past this many.
pub const TERMINAL_LIMIT: usize = 1 << 20;

/// What one hart reserved with LR: the address and length of the access, and
/// the write generation of its reservation granules at the time of the read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Reservation {
    address: u64,
    len: usize,
    generation: u64,
}

#[derive(Default)]
pub struct Profile {
    /// Function name -> (calls, exclusive operations, inclusive operations).
    pub functions: BTreeMap<String, (u64, u64, u64)>,
    children: Vec<u64>,
}

pub struct Interpreter<'a, M> {
    pub program: &'a Program,
    pub globals: Globals,
    pub memory: M,
    pub operations: u64,
    /// The value of `operations` past which execution fails with
    /// `Error::Limit`. `arm_operation_budget` moves it ahead of the counter.
    pub operation_limit: u64,
    /// The budget `arm_step_budget` arms: what one guest instruction may take.
    pub step_operation_budget: u64,
    pub diagnostics: std::collections::VecDeque<String>,
    /// The newest guest console output, at most `terminal_limit` bytes.
    pub terminal: std::collections::VecDeque<u8>,
    pub terminal_limit: usize,
    /// Console bytes dropped from the front of `terminal` to stay in the limit.
    pub terminal_dropped: u64,
    pub profile: Option<Profile>,
    pub memoize: bool,
    pub indexed: bool,
    indexed_cache: BTreeMap<String, std::sync::Arc<indexed::Compiled>>,
    memo: BTreeMap<String, memo::Memo>,
    depth: u32,
    reservation: Option<Reservation>,
    fetch_address: Option<u64>,
    /// The request of a physical access the host memory refused, kept until
    /// the model function that reports access faults turns it into one.
    host_fault: Option<Value>,
    decode_cache: BTreeMap<decode::CacheKey, Value>,
    decode_dependencies: decode::Dependencies,
    instruction_cache: BTreeMap<String, std::sync::Arc<Function>>,
    csr_cache: BTreeMap<u16, std::sync::Arc<Function>>,
    recorder: Option<record::Recorder>,
    watch_store: Option<u64>,
    watch_hit: bool,
    source_cache: BTreeMap<String, std::sync::Arc<record::Source>>,
    trace_completed: u32,
    address_arguments: BTreeMap<String, std::collections::BTreeSet<usize>>,
}

impl<'a, M: Memory> Interpreter<'a, M> {
    pub fn new(program: &'a Program, memory: M) -> Self {
        Self {
            program,
            globals: Globals::default(),
            memory,
            operations: 0,
            operation_limit: STEP_OPERATION_BUDGET,
            step_operation_budget: STEP_OPERATION_BUDGET,
            diagnostics: std::collections::VecDeque::new(),
            terminal: std::collections::VecDeque::new(),
            terminal_limit: TERMINAL_LIMIT,
            terminal_dropped: 0,
            profile: None,
            memoize: true,
            indexed: true,
            indexed_cache: BTreeMap::new(),
            memo: memo::Memo::analyze(program),
            depth: 0,
            reservation: None,
            fetch_address: None,
            host_fault: None,
            decode_cache: BTreeMap::new(),
            decode_dependencies: decode::Dependencies::new(program),
            instruction_cache: BTreeMap::new(),
            csr_cache: BTreeMap::new(),
            recorder: None,
            watch_store: None,
            watch_hit: false,
            source_cache: BTreeMap::new(),
            trace_completed: 0,
            address_arguments: vmlab_hv_tcg_ir::address_arguments(program),
        }
    }

    pub fn initialize(&mut self) -> Result<()> {
        for (name, ty) in &self.program.globals {
            self.globals
                .insert(name.clone(), self.default_value(ty, 0)?);
        }
        self.globals
            .insert("have_exception".into(), Value::Bool(false));
        self.globals
            .insert("current_exception".into(), Value::Undefined);
        self.globals
            .insert("throw_location".into(), Value::String(String::new()));
        for variants in self.program.enums.values() {
            for variant in variants {
                self.globals
                    .insert(variant.clone(), Value::Enum(variant.clone()));
            }
        }
        for function in &self.program.initializers {
            self.run_function(function, Vec::new())?;
        }
        Ok(())
    }

    pub fn default_value(&self, ty: &Type, depth: u32) -> Result<Value> {
        if depth > 64 {
            return Ok(Value::Undefined);
        }
        Ok(match ty {
            Type::Unit => Value::Unit,
            Type::Bool => Value::Bool(false),
            Type::Bit => Value::bits(1, 0u8),
            Type::Integer(_) => Value::int(0),
            Type::Bits(width) => Value::bits(width.unwrap_or(0), 0u8),
            Type::String => Value::String(String::new()),
            Type::Enum(name) => self
                .program
                .enums
                .get(name)
                .and_then(|v| v.first())
                .map_or(Value::Undefined, |v| Value::Enum(v.clone())),
            Type::Struct(name) => {
                let fields = self
                    .program
                    .structs
                    .get(name)
                    .ok_or_else(|| Error::Type(format!("unknown struct {name}")))?;
                Value::Struct(
                    fields
                        .iter()
                        .map(|(n, t)| Ok((n.clone(), self.default_value(t, depth + 1)?)))
                        .collect::<Result<_>>()?,
                )
            }
            Type::Vector(_) | Type::List(_) => Value::Vector(Vec::new()),
            Type::FixedVector(n, ty) if *n <= 1_048_576 => {
                Value::Vector(vec![self.default_value(ty, depth + 1)?; *n as usize])
            }
            _ => Value::Undefined,
        })
    }

    fn lookup<'b>(&'b self, frame: &'b Frame, name: &str) -> Result<&'b Value> {
        frame
            .get(name)
            .or_else(|| self.globals.get(name))
            .ok_or_else(|| Error::Undefined(name.into()))
    }

    fn eval(&mut self, frame: &Frame, expr: &Expr) -> Result<Value> {
        Ok(match expr {
            Expr::Id(name) => self.lookup(frame, name)?.clone(),
            Expr::Integer(n, _) => Value::int(
                n.parse::<num_bigint::BigInt>()
                    .map_err(|_| Error::Type("integer literal".into()))?,
            ),
            Expr::Bits(bits) => {
                let radix = if bits.as_bytes()[1] == b'x' { 16 } else { 2 };
                let width = (bits.len() - 2) as u32 * if radix == 16 { 4 } else { 1 };
                Value::bits(
                    width,
                    if width == 0 {
                        BigUint::ZERO
                    } else {
                        BigUint::parse_bytes(&bits.as_bytes()[2..], radix)
                            .ok_or_else(|| Error::Type("bitvector literal".into()))?
                    },
                )
            }
            Expr::Bool(b) => Value::Bool(*b),
            Expr::String(s) => Value::String(s.clone()),
            Expr::Unit => Value::Unit,
            Expr::Undefined(_) => Value::Undefined,
            Expr::Ref(name) => Value::Ref(name.clone()),
            Expr::Struct(_, fields) => Value::Struct(
                fields
                    .iter()
                    .map(|(n, e)| Ok((n.clone(), self.eval(frame, e)?)))
                    .collect::<Result<_>>()?,
            ),
            Expr::Field(e, field) => self.eval(frame, e)?.field(field)?.clone(),
            // Jib's `is` is a constructor mismatch test, used to skip a case.
            Expr::Is(e, tag) => Value::Bool(
                !matches!(self.eval(frame, e)?, Value::Union(ref actual, _) if actual == tag),
            ),
            Expr::As(e, tag) => match self.eval(frame, e)? {
                Value::Union(actual, value) if actual == *tag => *value,
                _ => return Err(Error::Type(format!("expected constructor {tag}"))),
            },
            Expr::Op(name, width, args) => {
                let args = args
                    .iter()
                    .map(|e| self.eval(frame, e))
                    .collect::<Result<Vec<_>>>()?;
                self.intrinsic(name, *width, &args)?
            }
        })
    }

    fn assign(&mut self, frame: &mut Frame, location: &Location, value: Value) -> Result<()> {
        if let Location::Id(name) = location
            && !frame.contains_key(name)
            && self.globals.contains_key(name)
        {
            self.globals.insert(name.clone(), value);
            return Ok(());
        }
        fn path(location: &Location, fields: &mut Vec<String>) -> Option<String> {
            match location {
                Location::Id(name) => Some(name.clone()),
                Location::Field(base, field) => {
                    let name = path(base, fields)?;
                    fields.push(field.clone());
                    Some(name)
                }
                Location::Deref(_) => None,
            }
        }
        if let Location::Deref(base) = location {
            let mut fields = Vec::new();
            let name =
                path(base, &mut fields).ok_or_else(|| Error::Type("nested dereference".into()))?;
            let Value::Ref(target) = self.lookup(frame, &name)?.clone() else {
                return Err(Error::Type("expected reference".into()));
            };
            return self.assign(frame, &Location::Id(target), value);
        }
        let mut fields = Vec::new();
        let root = path(location, &mut fields)
            .ok_or_else(|| Error::Type("field through reference".into()))?;
        let entry = if frame.contains_key(&root) || !self.globals.contains_key(&root) {
            frame.entry(root).or_insert(Value::Undefined)
        } else {
            self.globals.get_mut(&root).expect("global exists")
        };
        let mut slot = entry;
        for field in fields {
            let Value::Struct(map) = slot else {
                return Err(Error::Type("assignment to non-struct field".into()));
            };
            slot = map
                .get_mut(&field)
                .ok_or_else(|| Error::Type(format!("missing field {field}")))?;
        }
        *slot = value;
        Ok(())
    }

    /// Arm the operation budget: the next `budget` IR operations may run, then
    /// execution fails with `Error::Limit`. A caller that runs guest
    /// instructions arms it per instruction, so the budget bounds one
    /// instruction's work and never the lifetime of the guest.
    pub fn arm_operation_budget(&mut self, budget: u64) {
        self.operation_limit = self.operations.saturating_add(budget);
    }

    /// Arm `step_operation_budget` for the guest instruction about to run.
    pub fn arm_step_budget(&mut self) {
        self.arm_operation_budget(self.step_operation_budget);
    }

    fn terminal_write(&mut self, byte: u8) {
        if self.terminal_limit == 0 {
            self.terminal_dropped += 1;
            return;
        }
        while self.terminal.len() >= self.terminal_limit {
            self.terminal.pop_front();
            self.terminal_dropped += 1;
        }
        self.terminal.push_back(byte);
    }

    /// Whether this hart's reservation is still intact: no write has landed in
    /// its reservation granules since the exclusive read.
    pub(crate) fn reservation_intact(&self) -> Option<Reservation> {
        self.reservation.filter(|reservation| {
            self.memory
                .reservation_generation(reservation.address, reservation.len)
                == reservation.generation
        })
    }

    pub fn call(&mut self, name: &str, args: Vec<Value>) -> Result<Value> {
        if !is_checked_access(name) {
            return self.call_model(name, args);
        }
        match self.call_model(name, args) {
            Err(error) => self.guest_access_fault(name, error),
            value => value,
        }
    }

    /// The result a checked physical access returns to the model when the host
    /// memory refused it: the model's own access fault for that kind of
    /// access, at the refused address. The model accepts an address from its
    /// region table, which knows nothing of what the host `Memory` holds, so
    /// without this a guest access outside host RAM is a host error and the
    /// hart can never pass the instruction. Any other error is returned as is.
    pub(crate) fn guest_access_fault(&mut self, function: &str, error: Error) -> Result<Value> {
        let (Some(address), Some(tag)) = (error.host_memory_fault(), checked_access_error(function))
        else {
            return Err(error);
        };
        let Some(access) = self.host_fault.take().as_ref().and_then(requested_access) else {
            return Err(error);
        };
        let exception = self.call_model("zaccessFaultFromAccessType", vec![access])?;
        Ok(Value::Union(
            tag.into(),
            Box::new(Value::Struct(BTreeMap::from([
                (format!("{ACCESS_ERROR_TUPLE}0"), Value::bits(64, address)),
                (format!("{ACCESS_ERROR_TUPLE}1"), exception),
            ]))),
        ))
    }

    /// One physical access of the model. A refusal by the host memory is
    /// remembered for `guest_access_fault`, and a recording ends before the
    /// access: its trace hands the access back to the interpreter.
    fn memory_call(&mut self, args: &[Value], write: bool, exclusive: bool) -> Result<Value> {
        let result = self.memory_operation(args, write, exclusive);
        if matches!(result, Err(Error::Memory(_))) {
            self.host_fault = args.first().cloned();
            if let Some(mut recorder) = self.recorder.take() {
                recorder.stop("host memory refused the access");
                self.recorder = Some(recorder);
            }
        }
        result
    }

    fn call_model(&mut self, name: &str, args: Vec<Value>) -> Result<Value> {
        if name == "zplat_term_write" {
            let byte = args
                .first()
                .ok_or_else(|| Error::Type("terminal argument".into()))?
                .u64()?;
            let byte = u8::try_from(byte).map_err(|_| Error::Type("terminal byte".into()))?;
            self.terminal_write(byte);
            return Ok(Value::Unit);
        }
        if name.starts_with("zriscv_") {
            return softfloat::execute(self.program, name, &args);
        }
        let function = self
            .program
            .functions
            .get(name)
            .ok_or_else(|| Error::Undefined(format!("function {name}")))?;
        if self.recorder.is_none() && self.memoize && self.memo.contains_key(name) {
            if let Some(value) = self
                .memo
                .get_mut(name)
                .unwrap()
                .lookup(&mut self.globals, &args)
            {
                self.operations += 1;
                if self.operations > self.operation_limit {
                    return Err(Error::Limit);
                }
                return Ok(value);
            }
            let result = self.run_cached(function, args.clone())?;
            self.memo
                .get_mut(name)
                .unwrap()
                .insert(&self.globals, args, result.clone());
            return Ok(result);
        }
        if name == "zread_CSR" {
            let csr = u16::try_from(
                args.first()
                    .ok_or_else(|| Error::Type("CSR argument".into()))?
                    .u64()?,
            )
            .map_err(|_| Error::Type("CSR index".into()))?;
            let builder = self
                .csr_cache
                .entry(csr)
                .or_insert_with(|| {
                    let mut builder = function.clone();
                    let argument = builder.args[0].0.clone();
                    vmlab_hv_tcg_ir::specialize_argument(
                        &mut builder,
                        &argument,
                        Expr::Bits(format!("0x{csr:03x}")),
                    );
                    builder.name = format!("zread_CSR:{csr}");
                    std::sync::Arc::new(builder)
                })
                .clone();
            return self.run_cached(&builder, args);
        }
        if name == "zexecute"
            && let Some(Value::Union(tag, _)) = args.first()
        {
            // AMO semantics contain separate generated reads and writes. Hold
            // the memory transaction across that sequence, including MMU work.
            let transaction = if matches!(tag.as_str(), "zAMO" | "zSSAMOSWAP") {
                self.memory.transaction_lock()
            } else {
                None
            };
            let _atomic = transaction.as_ref().map(|lock| lock.lock());
            let builder = self
                .instruction_cache
                .entry(tag.clone())
                .or_insert_with(|| {
                    let mut builder = function.clone();
                    vmlab_hv_tcg_ir::specialize_constructor(&mut builder, tag);
                    builder.name = format!("zexecute:{tag}");
                    std::sync::Arc::new(builder)
                })
                .clone();
            return self.run_cached(&builder, args);
        }
        if name == "zpt_walk" {
            // Serialize the complete PTE read/check/A-D update with RAM stores.
            let transaction = self.memory.transaction_lock();
            let _pte = transaction.as_ref().map(|lock| lock.lock());
            return self.run_cached(function, args);
        }
        if matches!(name, "zext_decode" | "zext_decode_compressed")
            && self.recorder.is_none()
            && let Some(address) = self.fetch_address
            && self.decode_dependencies.cacheable
        {
            let key = decode::CacheKey {
                address,
                generation: self.memory.generation(),
                compressed: name == "zext_decode_compressed",
                instruction: args[0].u64()? as u32,
                context: self.decode_dependencies.snapshot(&self.globals),
            };
            if let Some(value) = self.decode_cache.get(&key) {
                return Ok(value.clone());
            }
            let value = self.run_cached(function, args)?;
            if self.decode_cache.len() >= 4096 {
                self.decode_cache.clear();
            }
            // An exception or an observable decoder write must run again on
            // the next access; memoization must not suppress its effects.
            if key.context == self.decode_dependencies.snapshot(&self.globals)
                && key.generation == self.memory.generation()
            {
                self.decode_cache.insert(key, value.clone());
            }
            return Ok(value);
        }
        self.run_cached(function, args)
    }

    pub fn run_function(&mut self, function: &Function, args: Vec<Value>) -> Result<Value> {
        if self.depth >= 256 {
            return Err(Error::Limit);
        }
        if args.len() != function.args.len() {
            return Err(Error::Type(format!("{} argument count", function.name)));
        }
        let mut frame: Frame = function
            .args
            .iter()
            .map(|(n, _)| n.clone())
            .zip(args)
            .collect();
        frame.insert(
            "return".into(),
            if matches!(function.result, Type::Struct(_) | Type::List(_)) {
                self.default_value(&function.result, 0)?
            } else {
                Value::Undefined
            },
        );
        self.depth += 1;
        if let Some(mut recorder) = self.recorder.take() {
            let key = if self.depth == 1 {
                format!("root:{}", function.name)
            } else {
                function.name.clone()
            };
            if self.depth == 1
                && self
                    .source_cache
                    .get(&key)
                    .is_some_and(|source| !source.matches(function))
            {
                self.source_cache.remove(&key);
            }
            let source = self
                .source_cache
                .entry(key)
                .or_insert_with(|| std::sync::Arc::new(record::Source::new(function)))
                .clone();
            recorder.enter(self, source, &frame);
            self.recorder = Some(recorder);
        }
        let start = self.operations;
        if let Some(profile) = &mut self.profile {
            profile.children.push(0);
        }
        let result = self.run_frame(function, &mut frame);
        if let Some(mut recorder) = self.recorder.take() {
            recorder.leave(self, result.is_ok());
            self.recorder = Some(recorder);
        }
        if let Some(profile) = &mut self.profile {
            let inclusive = self.operations - start;
            let children = profile.children.pop().expect("profile frame");
            let counts = profile.functions.entry(function.name.clone()).or_default();
            counts.0 += 1;
            counts.1 += inclusive - children;
            counts.2 += inclusive;
            if let Some(parent) = profile.children.last_mut() {
                *parent += inclusive;
            }
        }
        self.depth -= 1;
        result
    }

    fn run_frame(&mut self, function: &Function, frame: &mut Frame) -> Result<Value> {
        self.run_frame_from(function, frame, 0)
    }

    fn run_frame_from(
        &mut self,
        function: &Function,
        frame: &mut Frame,
        mut pc: usize,
    ) -> Result<Value> {
        while let Some(op) = function.code.get(pc) {
            self.operations += 1;
            if self.operations > self.operation_limit {
                return Err(Error::Limit);
            }
            let mut folded_result = None;
            if let Some(mut recorder) = self.recorder.take() {
                recorder.before(self, frame, op, pc);
                folded_result = recorder.take_folded_result();
                self.recorder = Some(recorder);
            }
            let step = (|| -> Result<Option<usize>> {
                match op {
                    Op::Declare(name, ty, init) => {
                        // Jib aggregate declarations allocate their fields; a list starts nil.
                        let value = match init {
                            Some(e) => self.eval(frame, e)?,
                            None if matches!(ty, Type::List(_) | Type::Struct(_)) => {
                                self.default_value(ty, 0)?
                            }
                            None => Value::Undefined,
                        };
                        frame.insert(name.clone(), value);
                    }
                    Op::Assign(loc, expr) => {
                        let v = self.eval(frame, expr)?;
                        self.assign(frame, loc, v)?;
                    }
                    Op::Call(loc, target, args) => {
                        let args = args
                            .iter()
                            .map(|e| self.eval(frame, e))
                            .collect::<Result<Vec<_>>>()?;
                        let value = match target {
                            Target::Function(name) => {
                                if let Some(result) = folded_result.take() {
                                    result?
                                } else {
                                    self.call(name, args)?
                                }
                            }
                            Target::Intrinsic(name) => self.intrinsic(name, None, &args)?,
                            Target::Native { operation, width } => {
                                self.native(operation, *width, &args)?
                            }
                            Target::Constructor(name) => {
                                if args.len() != 1 {
                                    return Err(Error::Type("constructor arity".into()));
                                }
                                Value::Union(name.clone(), Box::new(args[0].clone()))
                            }
                            Target::Memory {
                                write,
                                exclusive,
                                fetch,
                                ..
                            } => {
                                if *fetch && self.fetch_address.is_none() {
                                    self.fetch_address = Some(args[2].u64()?);
                                }
                                self.memory_call(&args, *write, *exclusive)?
                            }
                        };
                        self.assign(frame, loc, value)?;
                    }
                    Op::Branch(expr, target) => {
                        if self.eval(frame, expr)?.boolean()? {
                            return Ok(Some(*target));
                        }
                    }
                    Op::Jump(target) => return Ok(Some(*target)),
                    // Jib arbitrary is an undefined return value, commonly on
                    // the exceptional path. The caller must still get to test
                    // have_exception and run its generated catch code.
                    Op::Return | Op::Arbitrary => return Ok(None),
                    Op::Raise(cause) => return Err(Error::Exception(cause.clone())),
                    Op::Specialize(_, _) => {}
                }
                Ok(Some(pc + 1))
            })();
            if step.is_ok()
                && let Some(mut recorder) = self.recorder.take()
            {
                recorder.after(self, frame, op, pc);
                self.recorder = Some(recorder);
            }
            match step {
                Ok(Some(next)) => pc = next,
                Ok(None) => return Ok(frame.remove("return").unwrap_or(Value::Unit)),
                Err(source) => {
                    return Err(Error::At {
                        function: function.name.clone(),
                        instruction: pc,
                        source: Box::new(source),
                    });
                }
            }
        }
        Err(Error::Type(format!("{} fell off IR", function.name)))
    }

    fn memory_operation(&mut self, args: &[Value], write: bool, exclusive: bool) -> Result<Value> {
        if args.len() != if write { 5 } else { 4 } {
            return Err(Error::Type("memory operation arity".into()));
        }
        let address = args[2].u64()?;
        let len = args[3].index()?;
        if len > 4096 {
            return Err(Error::Type("memory operation exceeds 4096 bytes".into()));
        }
        if write {
            let (_, bits) = args[4].bitvector()?;
            let mut bytes = bits.to_bytes_le();
            bytes.resize(len, 0);
            let result = if exclusive {
                match self.reservation.take() {
                    Some(reservation) if reservation.address == address => self
                        .memory
                        .store_conditional(address, &bytes[..len], reservation.generation)?,
                    _ => false,
                }
            } else {
                self.memory.write(address, &bytes[..len])?;
                true
            };
            self.reservation = None;
            if result
                && self.watch_store.is_some_and(|watch| {
                    watch
                        .checked_sub(address)
                        .is_some_and(|delta| delta < len as u64)
                })
            {
                self.watch_hit = true;
            }
            Ok(Value::Bool(result))
        } else {
            let mut bytes = vec![0; len];
            if exclusive {
                let generation = self.memory.load_reserved(address, &mut bytes)?;
                self.reservation = Some(Reservation {
                    address,
                    len,
                    generation,
                });
            } else {
                self.memory.read(address, &mut bytes)?;
            }
            Ok(Value::bits(len as u32 * 8, BigUint::from_bytes_le(&bytes)))
        }
    }
}
