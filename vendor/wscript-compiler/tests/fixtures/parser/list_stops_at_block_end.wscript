// A list must not recover past the `}` of the block holding it. The lexer
// suppresses newlines inside `(` and `[`, so without `}` in the follow set
// the next stop is the `{` of the *following* declaration — recovery eats
// the block's `}` and the whole declaration after it, and `later` goes
// missing from the tree with only a puzzling "cannot find function" to
// show for it. That is the silent-declaration-loss `starts_item` ended.

fn main() { let x = [1 2
}

fn later() -> int { 7 }

fn caller(a: int b: int) -> int { later() }
