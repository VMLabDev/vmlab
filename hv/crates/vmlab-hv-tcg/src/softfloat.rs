use crate::{Error, Result, Value};
use std::collections::BTreeMap;
use vmlab_hv_tcg_ir::{Program, Type};

// `NAMES`, sorted: an operation's position is the index the C wrapper switches on.
include!(concat!(env!("OUT_DIR"), "/softfloat_names.rs"));

unsafe extern "C" {
    fn hv_softfloat(
        operation: u32,
        p0: u64,
        p1: u64,
        p2: u64,
        p3: u64,
        flags: *mut u8,
    ) -> u64;
}

/// The dispatch index of a Sail SoftFloat primitive.
pub(crate) fn index(name: &str) -> Option<u32> {
    NAMES
        .binary_search(&name)
        .ok()
        .and_then(|index| u32::try_from(index).ok())
}

/// Runs one operation and returns its result and exception flags, or `None`
/// for an index outside the table.
pub(crate) fn raw(operation: u32, inputs: [u64; 4]) -> Option<(u64, u8)> {
    let mut flags = 0;
    // SAFETY: flags points to a writable byte, and the wrapper bounds the index
    // itself. SoftFloat's rounding mode and exception flags are thread-local
    // (THREAD_LOCAL in build.rs) and the wrapper sets both on entry, so calls
    // on other threads cannot observe or disturb this one.
    let value = unsafe {
        hv_softfloat(
            operation, inputs[0], inputs[1], inputs[2], inputs[3], &mut flags,
        )
    };
    (flags != 255).then_some((value, flags))
}

pub fn execute(program: &Program, name: &str, args: &[Value]) -> Result<Value> {
    let (types, result) = program
        .declarations
        .get(name)
        .ok_or_else(|| Error::Intrinsic(name.into()))?;
    if types.len() != args.len() || args.len() > 4 {
        return Err(Error::Type("SoftFloat argument count".into()));
    }
    let Type::Struct(tuple) = result else {
        return Err(Error::Type("SoftFloat result tuple".into()));
    };
    let fields = program
        .structs
        .get(tuple)
        .ok_or_else(|| Error::Type("SoftFloat result fields".into()))?;
    if fields.len() != 2 {
        return Err(Error::Type("SoftFloat result arity".into()));
    }
    let mut inputs = [0u64; 4];
    for (i, value) in args.iter().enumerate() {
        inputs[i] = match value {
            Value::Bool(b) => u64::from(*b),
            _ => value.u64()?,
        };
    }
    let (value, flags) = index(name)
        .and_then(|operation| raw(operation, inputs))
        .ok_or_else(|| Error::Intrinsic(name.into()))?;
    let mut output = BTreeMap::new();
    output.insert(fields[0].0.clone(), Value::bits(5, flags));
    output.insert(
        fields[1].0.clone(),
        match fields[1].1 {
            Type::Bool => Value::Bool(value != 0),
            Type::Bits(Some(w)) => Value::bits(w, value),
            _ => return Err(Error::Type("SoftFloat result type".into())),
        },
    );
    Ok(Value::Struct(output))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_name_dispatches_by_its_sorted_index() {
        assert!(NAMES.windows(2).all(|pair| pair[0] < pair[1]));
        for (position, name) in NAMES.iter().enumerate() {
            assert_eq!(index(name), Some(position as u32));
            assert!(raw(position as u32, [0; 4]).is_some(), "{name}");
        }
        assert_eq!(index("zriscv_f32Nothing"), None);
        assert_eq!(raw(NAMES.len() as u32, [0; 4]), None);
        assert_eq!(raw(u32::MAX, [0; 4]), None);
        // The index selects the operation the name says: 1.0 + 2.0 and 6.0 / 2.0.
        let one = u64::from(1f32.to_bits());
        let two = u64::from(2f32.to_bits());
        let six = u64::from(6f32.to_bits());
        let three = u64::from(3f32.to_bits());
        assert_eq!(
            raw(index("zriscv_f32Add").unwrap(), [0, one, two, 0]),
            Some((three, 0))
        );
        assert_eq!(
            raw(index("zriscv_f32Div").unwrap(), [0, six, two, 0]),
            Some((three, 0))
        );
        assert_eq!(
            raw(index("zriscv_f32Lt").unwrap(), [one, two, 0, 0]),
            Some((1, 0))
        );
    }

    #[test]
    fn rounding_mode_and_flags_are_per_thread() {
        // 1.0 + 2^-30 is inexact in binary32: toward zero gives 1.0, upward
        // the next value above it. Each thread must see only its own mode and
        // its own flags while the others run without any lock.
        let add = index("zriscv_f32Add").unwrap();
        let div = index("zriscv_f32Div").unwrap();
        let one = u64::from(1f32.to_bits());
        let tiny = 0x3080_0000u64;
        let zero = u64::from(0f32.to_bits());
        let cases: [(u32, [u64; 4], (u64, u8)); 4] = [
            (add, [1, one, tiny, 0], (0x3f80_0000, 0x01)),
            (add, [3, one, tiny, 0], (0x3f80_0001, 0x01)),
            (add, [0, one, one, 0], (0x4000_0000, 0x00)),
            (div, [0, one, zero, 0], (0x7f80_0000, 0x08)),
        ];
        std::thread::scope(|scope| {
            for (operation, inputs, expected) in cases {
                scope.spawn(move || {
                    for _ in 0..200_000 {
                        assert_eq!(raw(operation, inputs), Some(expected));
                    }
                });
            }
        });
    }
}
