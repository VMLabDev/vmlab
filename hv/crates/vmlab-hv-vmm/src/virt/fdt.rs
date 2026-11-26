//! A writer of flattened device trees, version 17 (the Devicetree
//! Specification, chapter 5: the header, the memory reservation block, the
//! structure block and the strings block).

const MAGIC: u32 = 0xd00d_feed;
const BEGIN_NODE: u32 = 1;
const END_NODE: u32 = 2;
const PROP: u32 = 3;
const END: u32 = 9;

/// A tree under construction. Nodes open and close in the order of the
/// calls, and a property belongs to the node that is open.
#[derive(Default)]
pub struct Fdt {
    structure: Vec<u8>,
    strings: Vec<u8>,
    /// The offset of each property name in `strings`.
    names: std::collections::BTreeMap<String, u32>,
    depth: usize,
}

impl Fdt {
    pub fn new() -> Self {
        Self::default()
    }

    fn word(&mut self, value: u32) {
        self.structure.extend_from_slice(&value.to_be_bytes());
    }

    fn pad(&mut self) {
        while !self.structure.len().is_multiple_of(4) {
            self.structure.push(0);
        }
    }

    /// Open the node `name`. The root has the empty name.
    pub fn begin(&mut self, name: &str) -> &mut Self {
        self.word(BEGIN_NODE);
        self.structure.extend_from_slice(name.as_bytes());
        self.structure.push(0);
        self.pad();
        self.depth += 1;
        self
    }

    /// Close the node that is open.
    pub fn end(&mut self) -> &mut Self {
        assert!(self.depth > 0, "no node is open");
        self.word(END_NODE);
        self.depth -= 1;
        self
    }

    /// A property of raw bytes.
    pub fn bytes(&mut self, name: &str, value: &[u8]) -> &mut Self {
        assert!(self.depth > 0, "a property needs an open node");
        let offset = match self.names.get(name) {
            Some(offset) => *offset,
            None => {
                let offset = self.strings.len() as u32;
                self.strings.extend_from_slice(name.as_bytes());
                self.strings.push(0);
                self.names.insert(name.to_string(), offset);
                offset
            }
        };
        self.word(PROP);
        self.word(value.len() as u32);
        self.word(offset);
        self.structure.extend_from_slice(value);
        self.pad();
        self
    }

    /// A property without a value.
    pub fn flag(&mut self, name: &str) -> &mut Self {
        self.bytes(name, &[])
    }

    /// A property of 32-bit cells.
    pub fn cells(&mut self, name: &str, cells: &[u32]) -> &mut Self {
        let value: Vec<u8> = cells.iter().flat_map(|cell| cell.to_be_bytes()).collect();
        self.bytes(name, &value)
    }

    /// A property of 64-bit values, each as two cells.
    pub fn cells64(&mut self, name: &str, values: &[u64]) -> &mut Self {
        let value: Vec<u8> = values.iter().flat_map(|cell| cell.to_be_bytes()).collect();
        self.bytes(name, &value)
    }

    /// A property of one string.
    pub fn string(&mut self, name: &str, value: &str) -> &mut Self {
        self.strings_list(name, &[value])
    }

    /// A property of strings, each with its terminator.
    pub fn strings_list(&mut self, name: &str, values: &[&str]) -> &mut Self {
        let mut value = Vec::new();
        for text in values {
            value.extend_from_slice(text.as_bytes());
            value.push(0);
        }
        self.bytes(name, &value)
    }

    /// The blob. Every node must be closed.
    pub fn finish(mut self) -> Vec<u8> {
        assert_eq!(self.depth, 0, "a node is still open");
        self.word(END);
        const HEADER: usize = 40;
        // One empty entry ends the memory reservation block.
        const RESERVATIONS: usize = 16;
        let structure_at = HEADER + RESERVATIONS;
        let strings_at = structure_at + self.structure.len();
        let total = strings_at + self.strings.len();
        let mut blob = Vec::with_capacity(total);
        for value in [
            MAGIC,
            total as u32,
            structure_at as u32,
            strings_at as u32,
            HEADER as u32,
            // Version 17, readable by a parser of version 16.
            17,
            16,
            // The boot CPU.
            0,
            self.strings.len() as u32,
            self.structure.len() as u32,
        ] {
            blob.extend_from_slice(&value.to_be_bytes());
        }
        blob.extend_from_slice(&[0; RESERVATIONS]);
        blob.extend_from_slice(&self.structure);
        blob.extend_from_slice(&self.strings);
        blob
    }
}

/// A read of a blob, for the tests of what was written: the value of the
/// property `name` of the node at `path` (`/` is the root).
pub fn property(blob: &[u8], path: &str, name: &str) -> Option<Vec<u8>> {
    let word = |at: usize| -> Option<u32> {
        Some(u32::from_be_bytes(blob.get(at..at + 4)?.try_into().ok()?))
    };
    if word(0)? != MAGIC {
        return None;
    }
    let mut at = word(8)? as usize;
    let strings = word(12)? as usize;
    let mut stack: Vec<String> = Vec::new();
    let want: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    loop {
        let token = word(at)?;
        at += 4;
        match token {
            BEGIN_NODE => {
                let len = blob.get(at..)?.iter().position(|byte| *byte == 0)?;
                stack.push(String::from_utf8_lossy(&blob[at..at + len]).into_owned());
                at = (at + len + 1).div_ceil(4) * 4;
            }
            END_NODE => {
                stack.pop();
            }
            PROP => {
                let len = word(at)? as usize;
                let name_at = strings + word(at + 4)? as usize;
                let value = blob.get(at + 8..at + 8 + len)?;
                let end = blob.get(name_at..)?.iter().position(|byte| *byte == 0)?;
                // The root is the first entry of the stack, with no name.
                if &blob[name_at..name_at + end] == name.as_bytes()
                    && stack.len() == want.len() + 1
                    && stack[1..].iter().map(String::as_str).eq(want.iter().copied())
                {
                    return Some(value.to_vec());
                }
                at = (at + 8 + len).div_ceil(4) * 4;
            }
            END => return None,
            // A NOP, or a token that this reader does not know.
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tree_reads_back_and_its_header_adds_up() {
        let mut fdt = Fdt::new();
        fdt.begin("");
        fdt.cells("#address-cells", &[2]);
        fdt.string("compatible", "linux,dummy-virt");
        fdt.begin("memory@40000000");
        fdt.string("device_type", "memory");
        fdt.cells64("reg", &[0x4000_0000, 0x8000_0000]);
        fdt.end();
        fdt.begin("chosen");
        fdt.flag("empty");
        fdt.strings_list("list", &["a", "bc"]);
        fdt.end();
        fdt.end();
        let blob = fdt.finish();
        let word = |at: usize| u32::from_be_bytes(blob[at..at + 4].try_into().unwrap());
        assert_eq!(word(0), 0xd00d_feed);
        assert_eq!(word(4) as usize, blob.len());
        assert_eq!(word(8) + word(36), word(12));
        assert_eq!(word(12) + word(32), word(4));
        assert_eq!(word(20), 17);
        assert_eq!(
            property(&blob, "/", "compatible").unwrap(),
            b"linux,dummy-virt\0"
        );
        assert_eq!(
            property(&blob, "/memory@40000000", "reg").unwrap(),
            [0, 0, 0, 0, 0x40, 0, 0, 0, 0, 0, 0, 0, 0x80, 0, 0, 0]
        );
        assert_eq!(property(&blob, "/chosen", "empty").unwrap(), b"");
        assert_eq!(property(&blob, "/chosen", "list").unwrap(), b"a\0bc\0");
        assert_eq!(property(&blob, "/chosen", "none"), None);
        assert_eq!(property(&blob, "/none", "reg"), None);
    }
}
