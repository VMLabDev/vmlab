//! `generated/sysreg.rs`: the system register and system operation tables.

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use std::fmt::Write as _;

fn bits(value: &Value, key: &str) -> Result<u32> {
    let list = value[key]
        .as_array()
        .ok_or_else(|| anyhow!("no bit list `{key}`"))?;
    let mut result = 0u32;
    for (position, bit) in list.iter().enumerate() {
        result |= (bit.as_u64().context("a bit that is not fixed")? as u32 & 1) << position;
    }
    Ok(result)
}

fn name(value: &Value) -> Result<&str> {
    value["Name"].as_str().context("no Name")
}

/// `{AArch64::FeatureA, AArch64::FeatureB}` as `["FeatureA", "FeatureB"]`.
fn requires(value: &Value) -> Result<String> {
    let text = value["Requires"].as_str().context("no Requires")?;
    let inner = text.trim().trim_start_matches('{').trim_end_matches('}');
    let mut names = Vec::new();
    for item in inner
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
    {
        let Some(feature) = item.strip_prefix("AArch64::") else {
            bail!("the Requires entry {item:?} is not of the form AArch64::Name");
        };
        names.push(format!("{feature:?}"));
    }
    Ok(format!("&[{}]", names.join(", ")))
}

fn instances<'a>(json: &'a Value, class: &str) -> Result<Vec<&'a Value>> {
    json["!instanceof"][class]
        .as_array()
        .with_context(|| format!("the dump has no {class} records"))?
        .iter()
        .map(|id| {
            let id = id.as_str().context("record name")?;
            Ok(&json[id])
        })
        .collect()
}

pub fn generate(json: &Value, sha256: &str) -> Result<String> {
    let mut out = String::from(crate::NOTICE);
    out.push_str(
        "//\n// Input: the JSON dump of AArch64.td by the pinned llvm-tblgen\n\
         // (hv/toolchain/llvm/pins.env): the records of the classes SysReg, TLBI,\n\
         // DC, AT, IC, PStateImm0_15, PStateImm0_1 and SVCR, of every feature.\n\n#![allow(clippy::all)]\n\n",
    );
    let _ = writeln!(
        out,
        "/// sha256 of the JSON dump that this file was generated from.\n\
         pub const TABLES_SHA256: &str = {sha256:?};\n"
    );
    out.push_str(
        "/// A system register. `encoding` is op0:op1:CRn:CRm:op2, 16 bits.\n\
         #[derive(Clone, Copy, Debug, PartialEq, Eq)]\n\
         pub struct SysReg {\n    pub name: &'static str,\n    pub encoding: u16,\n    \
         pub readable: bool,\n    pub writeable: bool,\n    \
         /// The LLVM features or architecture versions that it needs. Empty: none.\n    \
         pub requires: &'static [&'static str],\n}\n\n\
         /// A named operation with an encoding. For `DC` and `AT` the encoding\n\
         /// is op1:CRn:CRm:op2 of `SYS`, 14 bits. For a PSTATE field it is the\n\
         /// `pstatefield` of the MSR record that takes it, and for `SVCR` the\n\
         /// three bits of its operand.\n\
         #[derive(Clone, Copy, Debug, PartialEq, Eq)]\n\
         pub struct SysOp {\n    pub name: &'static str,\n    pub encoding: u16,\n    \
         pub requires: &'static [&'static str],\n}\n\n\
         /// An instruction cache operation. `needs_reg`: it takes a register.\n\
         #[derive(Clone, Copy, Debug, PartialEq, Eq)]\n\
         pub struct IcOp {\n    pub name: &'static str,\n    pub encoding: u16,\n    pub needs_reg: bool,\n}\n\n\
         /// A TLB invalidation. The tables give op1, CRm and op2. CRn is not in\n\
         /// the record. `reg_use` is LLVM's `REG_REQUIRED`, `REG_OPTIONAL` or `REG_NONE`.\n\
         #[derive(Clone, Copy, Debug, PartialEq, Eq)]\n\
         pub struct TlbiOp {\n    pub name: &'static str,\n    pub op1: u8,\n    pub crm: u8,\n    \
         pub op2: u8,\n    pub has_tlbip: bool,\n    pub reg_use: &'static str,\n}\n\n",
    );

    let mut registers = Vec::new();
    for record in instances(json, "SysReg")? {
        registers.push((
            bits(record, "Encoding")?,
            name(record)?.to_string(),
            record["Readable"].as_i64() == Some(1),
            record["Writeable"].as_i64() == Some(1),
            requires(record)?,
        ));
    }
    registers.sort();
    let _ = writeln!(
        out,
        "/// Every `SysReg` record, sorted by encoding and then by name.\n\
         pub static SYSREGS: [SysReg; {}] = [",
        registers.len()
    );
    for (encoding, name, readable, writeable, requires) in &registers {
        if *encoding > 0xffff {
            bail!("{name}: the encoding does not fit 16 bits");
        }
        let _ = writeln!(
            out,
            "    SysReg {{ name: {name:?}, encoding: {encoding:#06x}, readable: {readable}, \
             writeable: {writeable}, requires: {requires} }},"
        );
    }
    out.push_str("];\n\n");

    for (class, table) in [
        ("DC", "DC_OPS"),
        ("AT", "AT_OPS"),
        ("PStateImm0_15", "PSTATE_IMM0_15_OPS"),
        ("PStateImm0_1", "PSTATE_IMM0_1_OPS"),
        ("SVCR", "SVCR_OPS"),
    ] {
        let mut operations = Vec::new();
        for record in instances(json, class)? {
            operations.push((
                bits(record, "Encoding")?,
                name(record)?.to_string(),
                requires(record)?,
            ));
        }
        operations.sort();
        let _ = writeln!(
            out,
            "/// Every `{class}` record, sorted by encoding.\npub static {table}: [SysOp; {}] = [",
            operations.len()
        );
        for (encoding, name, requires) in &operations {
            let _ = writeln!(
                out,
                "    SysOp {{ name: {name:?}, encoding: {encoding:#06x}, requires: {requires} }},"
            );
        }
        out.push_str("];\n\n");
    }

    let mut operations = Vec::new();
    for record in instances(json, "IC")? {
        operations.push((
            bits(record, "Encoding")?,
            name(record)?.to_string(),
            record["NeedsReg"].as_i64() == Some(1),
        ));
    }
    operations.sort();
    let _ = writeln!(
        out,
        "/// Every `IC` record, sorted by encoding.\npub static IC_OPS: [IcOp; {}] = [",
        operations.len()
    );
    for (encoding, name, needs_reg) in &operations {
        let _ = writeln!(
            out,
            "    IcOp {{ name: {name:?}, encoding: {encoding:#06x}, needs_reg: {needs_reg} }},"
        );
    }
    out.push_str("];\n\n");

    let mut operations = Vec::new();
    for record in instances(json, "TLBI")? {
        operations.push((
            bits(record, "Op1")?,
            bits(record, "CRm")?,
            bits(record, "Op2")?,
            name(record)?.to_string(),
            record["HasTLBIP"].as_i64() == Some(1),
            record["RegUse"]["def"]
                .as_str()
                .context("RegUse")?
                .to_string(),
        ));
    }
    operations.sort();
    let _ = writeln!(
        out,
        "/// Every `TLBI` record, sorted by op1, CRm, op2 and name.\npub static TLBI_OPS: [TlbiOp; {}] = [",
        operations.len()
    );
    for (op1, crm, op2, name, has_tlbip, reg_use) in &operations {
        let _ = writeln!(
            out,
            "    TlbiOp {{ name: {name:?}, op1: {op1}, crm: {crm}, op2: {op2}, has_tlbip: {has_tlbip}, \
             reg_use: {reg_use:?} }},"
        );
    }
    out.push_str("];\n");
    Ok(out)
}
