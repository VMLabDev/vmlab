use super::{Binary, Graph, Operation, ScalarId, ScalarType, Unary};

/// How many operations deep `modular` pushes a reduction. Each level goes
/// into both operands, and an operand can be shared, so a chain of `x + x`
/// takes two to the power of its length without a bound: a superblock that
/// holds many turns of such a loop (3e DESIGN.md J14) took minutes to
/// translate. Past the bound the reduction stays a truncation of its own.
const MOST_DEPTH: u32 = 8;

impl Graph {
    /// Push a final modular reduction through arithmetic and identity views.
    /// A narrower intermediate reduction must remain visible.
    pub(super) fn modular(&mut self, value: ScalarId, width: u16) -> Option<ScalarId> {
        if self.modular_depth >= MOST_DEPTH {
            return None;
        }
        self.modular_depth += 1;
        let reduced = self.modular_within(value, width);
        self.modular_depth -= 1;
        reduced
    }

    fn modular_within(&mut self, value: ScalarId, width: u16) -> Option<ScalarId> {
        let ty = ScalarType::Bits(width);
        let mut source = value;
        while let Operation::Refine(previous) = self.scalars[source].operation {
            if self.scalars[previous].ty == ty {
                return Some(self.resolve(previous));
            }
            source = previous;
        }
        if !matches!(self.scalars[source].ty, ScalarType::Integer)
            && !matches!(self.scalars[source].ty, ScalarType::Bits(bits) if bits >= width)
        {
            return None;
        }
        let Operation::Binary(operation, a, b) = self.scalars[source].operation else {
            return None;
        };
        if !matches!(
            operation,
            Binary::Add | Binary::Sub | Binary::Mul | Binary::And | Binary::Or | Binary::Xor
        ) {
            return None;
        }
        let a = self.unary(ty, Unary::Truncate(width), a)?;
        let b = self.unary(ty, Unary::Truncate(width), b)?;
        self.binary(ty, operation, a, b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integer_roundtrips_collapse_repeated_modular_register_updates() {
        let mut graph = Graph::default();
        let ty = ScalarType::Bits(64);
        let input = graph.input(0, ty, None);
        let one = graph.constant(ScalarType::Integer, 1);
        let zero = graph.constant(ScalarType::Integer, 0);
        let mut value = input;
        for _ in 0..10 {
            let unsigned = graph
                .binary(ScalarType::Integer, Binary::Add, value, zero)
                .unwrap();
            let sum = graph
                .binary(ScalarType::Integer, Binary::Add, unsigned, one)
                .unwrap();
            let wide = graph
                .unary(ScalarType::Bits(128), Unary::Truncate(128), sum)
                .unwrap();
            value = graph.slice(wide, 0, 64);
        }
        let ten = graph.constant(ty, 10);
        let expected = graph.binary(ty, Binary::Add, input, ten).unwrap();
        assert_eq!(value, expected);
    }

    #[test]
    fn a_reduction_of_a_long_chain_of_shared_operands_ends() {
        // x = x + x a hundred times, then the low 32 bits: the reduction
        // goes into both operands of each sum, which are one scalar.
        let mut graph = Graph::default();
        let ty = ScalarType::Bits(64);
        let mut value = graph.input(0, ty, None);
        for _ in 0..100 {
            value = graph.binary(ty, Binary::Add, value, value).unwrap();
        }
        let low = graph
            .unary(ScalarType::Bits(32), Unary::Truncate(32), value)
            .unwrap();
        assert_eq!(graph.scalars[low].ty, ScalarType::Bits(32));
    }
}
