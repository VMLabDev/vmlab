//! One Cranelift function per block.

use crate::{Block, Code, OFF_TLB, PC, TLB_ENTRIES, TLB_MASK, slow_load, slow_store};
use cranelift_codegen::Context;
use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::types::I64;
use cranelift_codegen::ir::{AbiParam, BlockArg, InstBuilder, MemFlagsData, Value};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{Module, default_libcall_names};

pub struct Jit {
    module: JITModule,
    context: Context,
    builder: FunctionBuilderContext,
}

impl Jit {
    pub fn new(level: &str) -> Self {
        let mut flags = settings::builder();
        flags.set("opt_level", level).unwrap();
        flags.set("enable_verifier", "false").unwrap();
        let isa = cranelift_native::builder().unwrap().finish(settings::Flags::new(flags)).unwrap();
        let module = JITModule::new(JITBuilder::with_isa(isa, default_libcall_names()));
        let context = module.make_context();
        Self { module, context, builder: FunctionBuilderContext::new() }
    }

    /// The entry of the compiled block and the bytes of its code.
    pub fn compile(&mut self, block: &Block) -> (*const u8, usize) {
        self.module.clear_context(&mut self.context);
        self.context.func.signature.params.push(AbiParam::new(I64));
        let mut load_signature = self.module.make_signature();
        load_signature.params.extend([AbiParam::new(I64), AbiParam::new(I64)]);
        load_signature.returns.push(AbiParam::new(I64));
        let mut store_signature = self.module.make_signature();
        store_signature.params.extend([AbiParam::new(I64); 3]);

        let target = self.module.target_config();
        let mut b = FunctionBuilder::new(&mut self.context.func, &mut self.builder);
        let load_signature = b.import_signature(load_signature);
        let store_signature = b.import_signature(store_signature);
        let entry = b.create_block();
        b.append_block_params_for_function_params(entry);
        b.switch_to_block(entry);
        b.seal_block(entry);
        let state = b.block_params(entry)[0];
        let flags = MemFlagsData::trusted();

        let mut values: Vec<Option<Value>> = vec![None; block.slots];
        for (slot, value) in &block.consts {
            values[*slot as usize] = Some(b.ins().iconst(I64, *value as i64));
        }
        for (slot, guest) in &block.inputs {
            values[*slot as usize] = Some(b.ins().load(I64, flags, state, *guest as i32 * 8));
        }
        // The host address of the address `va`, or a jump to `slow`.
        let lookup = |b: &mut FunctionBuilder, va: Value, slow| {
            let index = b.ins().ushr_imm(va, 12);
            let index = b.ins().band_imm(index, TLB_ENTRIES as i64 - 1);
            let offset = b.ins().ishl_imm(index, 4);
            let table = b.ins().load(I64, flags, state, OFF_TLB);
            let entry = b.ins().iadd(table, offset);
            let tag = b.ins().load(I64, flags, entry, 0);
            let masked = b.ins().band_imm(va, i64::from(TLB_MASK));
            let hit = b.ins().icmp(IntCC::Equal, masked, tag);
            let fast = b.create_block();
            b.ins().brif(hit, fast, &[], slow, &[]);
            b.switch_to_block(fast);
            b.seal_block(fast);
            let addend = b.ins().load(I64, flags, entry, 8);
            b.ins().iadd(va, addend)
        };
        for op in &block.ops {
            let a = values[op.a as usize].unwrap();
            let second = values[op.b as usize];
            let compare = |b: &mut FunctionBuilder, cc| {
                let bit = b.ins().icmp(cc, a, second.unwrap());
                b.ins().uextend(I64, bit)
            };
            let value = match op.code {
                Code::Add => b.ins().iadd(a, second.unwrap()),
                Code::Sub => b.ins().isub(a, second.unwrap()),
                Code::And => b.ins().band(a, second.unwrap()),
                Code::Or => b.ins().bor(a, second.unwrap()),
                Code::Xor => b.ins().bxor(a, second.unwrap()),
                Code::Shr => b.ins().ushr_imm(a, i64::from(op.aux)),
                Code::LessEq => compare(&mut b, IntCC::UnsignedLessThanOrEqual),
                Code::Less => compare(&mut b, IntCC::UnsignedLessThan),
                Code::IsZero => {
                    let bit = b.ins().icmp_imm(IntCC::Equal, a, 0);
                    b.ins().uextend(I64, bit)
                }
                Code::Load8 => {
                    let slow = b.create_block();
                    let done = b.create_block();
                    b.append_block_param(done, I64);
                    b.set_cold_block(slow);
                    let host = lookup(&mut b, a, slow);
                    let fast = b.ins().load(I64, flags, host, 0);
                    b.ins().jump(done, &[BlockArg::Value(fast)]);
                    b.switch_to_block(slow);
                    b.seal_block(slow);
                    let helper = b.ins().iconst(I64, slow_load as *const () as usize as i64);
                    let call = b.ins().call_indirect(load_signature, helper, &[state, a]);
                    let result = b.inst_results(call)[0];
                    b.ins().jump(done, &[BlockArg::Value(result)]);
                    b.switch_to_block(done);
                    b.seal_block(done);
                    b.block_params(done)[0]
                }
                Code::Store8 => {
                    let slow = b.create_block();
                    let done = b.create_block();
                    b.set_cold_block(slow);
                    let host = lookup(&mut b, a, slow);
                    b.ins().store(flags, second.unwrap(), host, 0);
                    b.ins().jump(done, &[]);
                    b.switch_to_block(slow);
                    b.seal_block(slow);
                    let helper = b.ins().iconst(I64, slow_store as *const () as usize as i64);
                    b.ins().call_indirect(store_signature, helper, &[state, a, second.unwrap()]);
                    b.ins().jump(done, &[]);
                    b.switch_to_block(done);
                    b.seal_block(done);
                    continue;
                }
            };
            values[op.dst as usize] = Some(value);
        }
        for (guest, slot) in &block.outputs {
            b.ins().store(flags, values[*slot as usize].unwrap(), state, *guest as i32 * 8);
        }
        let taken = b.ins().iconst(I64, block.taken as i64);
        let fall = b.ins().iconst(I64, block.fall as i64);
        let next = b.ins().select(values[block.cond as usize].unwrap(), taken, fall);
        b.ins().store(flags, next, state, PC as i32 * 8);
        b.ins().return_(&[]);
        b.finalize(target);

        let id = self.module.declare_anonymous_function(&self.context.func.signature).unwrap();
        self.module.define_function(id, &mut self.context).unwrap();
        let size = self.context.compiled_code().unwrap().code_info().total_size as usize;
        self.module.finalize_definitions().unwrap();
        (self.module.get_finalized_function(id), size)
    }
}
