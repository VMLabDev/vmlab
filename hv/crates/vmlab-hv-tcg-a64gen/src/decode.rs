//! `generated/decode.rs`: the records of the selected sets, one struct of raw
//! fields per format class, and a decision tree over the fixed bits.

use crate::tables::Config;
use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

/// Superclasses that every instruction record has, or that say nothing
/// about its encoding. The format class of a record is its last superclass
/// that is not one of these.
const COMMON_SUPERCLASSES: &[&str] = &[
    "InstructionEncoding",
    "Instruction",
    "AArch64Inst",
    "EncodedI",
    "I",
    "Sched",
    "NormalizedBadReg",
];

/// A subtree with at least this many candidates becomes a function of its own.
const SPLIT: usize = 24;

type Parts = Vec<(u8, u8, u8)>;

struct Record {
    name: String,
    mask: u32,
    value: u32,
    fallback: bool,
    features: u16,
    format: String,
    /// Field name in snake case, to its parts (word bit, field bit, length).
    fields: BTreeMap<String, Parts>,
    constraint: Option<String>,
    layout: usize,
    priority: usize,
}

pub struct Output {
    pub source: String,
    pub records: usize,
    pub formats: usize,
    pub constraints: usize,
}

fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key]
        .as_str()
        .ok_or_else(|| anyhow!("the record has no string `{key}`"))
}

fn flag(value: &Value, key: &str) -> bool {
    value[key].as_i64() == Some(1)
}

/// Evaluate an `AssemblerCondDag` with the selected features on and every
/// other feature (also `FeatureAll`) off. `mentioned` collects the features.
fn predicate(dag: &Value, selected: &[String], mentioned: &mut BTreeSet<String>) -> Result<bool> {
    if dag["kind"] == "def" {
        let name = text(dag, "def")?;
        let Some(feature) = name.strip_prefix("Feature") else {
            bail!("the predicate operand {name} is not a feature");
        };
        if feature == "All" {
            return Ok(false);
        }
        mentioned.insert(feature.to_string());
        return Ok(selected.iter().any(|have| have == feature));
    }
    let operator = text(&dag["operator"], "def")?;
    let mut results = Vec::new();
    for argument in dag["args"].as_array().context("dag without args")? {
        results.push(predicate(&argument[0], selected, mentioned)?);
    }
    Ok(match operator {
        "all_of" => results.iter().all(|value| *value),
        "any_of" => results.iter().any(|value| *value),
        other => bail!("the predicate operator {other} is not known to the generator"),
    })
}

fn snake(field: &str) -> String {
    let name = field.to_ascii_lowercase();
    match name.as_str() {
        "type" | "mod" | "ref" | "match" | "loop" | "move" | "in" | "fn" | "as" | "use" => {
            format!("{name}_")
        }
        _ => name,
    }
}

fn parse_record(
    json: &Value,
    name: &str,
    config: &Config,
    unselected: &mut usize,
) -> Result<Option<Record>> {
    let record = &json[name];
    if flag(record, "isPseudo") || flag(record, "isCodeGenOnly") {
        return Ok(None);
    }
    let inst = record["Inst"]
        .as_array()
        .with_context(|| format!("{name} has no Inst"))?;
    if inst.len() != 32 {
        bail!("{name}: Inst has {} bits", inst.len());
    }
    let (mut mask, mut value) = (0u32, 0u32);
    let mut bits: BTreeMap<String, Vec<(u8, u8)>> = BTreeMap::new();
    for (position, bit) in inst.iter().enumerate() {
        let position = position as u8;
        match bit {
            Value::Number(number) => {
                mask |= 1 << position;
                value |= (number.as_u64().context("fixed bit")? as u32 & 1) << position;
            }
            // An unset bit: the tables give it no meaning.
            Value::Null => {}
            Value::Object(_) => {
                let variable = snake(text(bit, "var")?);
                let index = match text(bit, "kind")? {
                    "varbit" => bit["index"].as_u64().context("varbit index")? as u8,
                    "var" => 0,
                    other => bail!("{name}: bit {position} has the kind {other}"),
                };
                bits.entry(variable).or_default().push((position, index));
            }
            other => bail!("{name}: bit {position} is {other}"),
        }
    }
    if mask == 0 {
        return Ok(None);
    }
    let mut holds = true;
    let mut mentioned = BTreeSet::new();
    for item in record["Predicates"].as_array().context("Predicates")? {
        let predicate_name = text(item, "def")?;
        let dag = &json[predicate_name]["AssemblerCondDag"];
        if dag.is_null() {
            bail!("{name}: the predicate {predicate_name} has no AssemblerCondDag");
        }
        holds &= predicate(dag, &config.features, &mut mentioned)?;
    }
    if !holds {
        *unselected += 1;
        return Ok(None);
    }
    let mut features = 0u16;
    for feature in &mentioned {
        if let Some(index) = config.features.iter().position(|have| have == feature) {
            features |= 1 << index;
        }
    }
    let fallback = match text(record, "DecoderNamespace")? {
        "" => false,
        "Fallback" => true,
        other => bail!("{name}: the decoder namespace {other} is not known to the generator"),
    };
    let format = record["!superclasses"]
        .as_array()
        .context("superclasses")?
        .iter()
        .filter_map(Value::as_str)
        .rfind(|class| !COMMON_SUPERCLASSES.contains(class))
        .unwrap_or(name)
        .to_string();
    let mut fields = BTreeMap::new();
    for (field, mut list) in bits {
        list.sort_by_key(|(_, index)| *index);
        let mut parts: Parts = Vec::new();
        for (word, index) in list {
            match parts.last_mut() {
                Some((word_lsb, field_lsb, len))
                    if *word_lsb + *len == word && *field_lsb + *len == index =>
                {
                    *len += 1
                }
                _ => parts.push((word, index, 1)),
            }
        }
        fields.insert(field, parts);
    }
    Ok(Some(Record {
        name: name.to_string(),
        mask,
        value,
        fallback,
        features,
        format,
        fields,
        constraint: None,
        layout: 0,
        priority: 0,
    }))
}

fn overlap(a: &Record, b: &Record) -> bool {
    let common = a.mask & b.mask;
    a.value & common == b.value & common
}

/// Order the records as the decoder tries them (G1b) and return that order.
fn prioritise(records: &mut [Record], config: &Config) -> Result<Vec<usize>> {
    let key = |record: &Record| (record.fallback, std::cmp::Reverse(record.mask.count_ones()));
    let mut order: Vec<usize> = (0..records.len()).collect();
    order.sort_by(|a, b| {
        key(&records[*a])
            .cmp(&key(&records[*b]))
            .then_with(|| records[*a].name.cmp(&records[*b].name))
    });
    let index: BTreeMap<&str, usize> = records
        .iter()
        .enumerate()
        .map(|(id, record)| (record.name.as_str(), id))
        .collect();
    let mut decided = BTreeSet::new();
    for entry in &config.overrides {
        let (Some(&winner), Some(&loser)) = (
            index.get(entry.winner.as_str()),
            index.get(entry.loser.as_str()),
        ) else {
            bail!(
                "overrides.toml names {} and {}: one is not a selected record",
                entry.winner,
                entry.loser
            );
        };
        if key(&records[winner]) != key(&records[loser])
            || !overlap(&records[winner], &records[loser])
        {
            bail!(
                "overrides.toml: {} and {} do not need an override",
                entry.winner,
                entry.loser
            );
        }
        let (at_winner, at_loser) = (
            order.iter().position(|id| *id == winner).unwrap_or(0),
            order.iter().position(|id| *id == loser).unwrap_or(0),
        );
        if at_winner > at_loser {
            order.swap(at_winner, at_loser);
        }
        decided.insert((winner.min(loser), winner.max(loser)));
    }
    // Records of one key are next to each other in the order.
    let mut undecided = Vec::new();
    for (position, &a) in order.iter().enumerate() {
        for &b in &order[position + 1..] {
            if key(&records[a]) != key(&records[b]) {
                break;
            }
            if overlap(&records[a], &records[b]) && !decided.contains(&(a.min(b), a.max(b))) {
                undecided.push(format!("{} and {}", records[a].name, records[b].name));
            }
        }
    }
    if !undecided.is_empty() {
        bail!(
            "overlapping records of equal specificity need an entry in overrides.toml: {}",
            undecided.join(", ")
        );
    }
    for (priority, id) in order.iter().enumerate() {
        records[*id].priority = priority;
    }
    Ok(order)
}

struct Tree<'a> {
    records: &'a [Record],
    functions: Vec<String>,
}

impl Tree<'_> {
    fn leaf(&self, id: usize) -> String {
        format!("leaf(Record::{}, w)", self.records[id].name)
    }

    /// An expression of type `Option<Decoded>` for the candidates, which are
    /// in priority order. `tested` holds the bits that the path has fixed.
    fn node(&mut self, candidates: &[usize], tested: u32, depth: usize, top: bool) -> String {
        if candidates.len() >= SPLIT && !top {
            let body = self.node(candidates, tested, 1, true);
            let id = self.functions.len();
            self.functions.push(String::new());
            self.functions[id] =
                format!("fn t{id}(w: u32) -> Option<Decoded> {{\n    {body}\n}}\n");
            return format!("t{id}(w)");
        }
        let pad = "    ".repeat(depth);
        let remaining = |id: usize| self.records[id].mask & !tested;
        if candidates.is_empty() {
            return "None".into();
        }
        if candidates.iter().all(|id| remaining(*id) == 0) {
            // Every candidate matches the fixed bits. A candidate without a
            // constraint always decodes, so nothing after it is reachable.
            let mut expression = String::new();
            for (position, id) in candidates.iter().enumerate() {
                if position > 0 {
                    expression.push_str(".or_else(|| ");
                }
                expression.push_str(&self.leaf(*id));
                if position > 0 {
                    expression.push(')');
                }
                if self.records[*id].constraint.is_none() {
                    break;
                }
            }
            return expression;
        }
        let common = candidates
            .iter()
            .fold(u32::MAX, |bits, id| bits & remaining(*id));
        if common != 0 {
            let mut groups: BTreeMap<u32, Vec<usize>> = BTreeMap::new();
            for id in candidates {
                groups
                    .entry(self.records[*id].value & common)
                    .or_default()
                    .push(*id);
            }
            let mut expression = format!("match w & {common:#010x} {{\n");
            for (value, group) in groups {
                let child = self.node(&group, tested | common, depth + 1, false);
                let _ = writeln!(expression, "{pad}    {value:#010x} => {child},");
            }
            let _ = write!(expression, "{pad}    _ => None,\n{pad}}}");
            return expression;
        }
        // No bit is fixed by every candidate: split on the bit that most of
        // them fix. A candidate that leaves it free goes to both sides and
        // keeps its place in the order.
        let bit = (0..32u32)
            .rev()
            .max_by_key(|bit| {
                candidates
                    .iter()
                    .filter(|id| remaining(**id) & (1 << bit) != 0)
                    .count()
            })
            .unwrap_or(0);
        let side = |set: bool| -> Vec<usize> {
            candidates
                .iter()
                .copied()
                .filter(|id| {
                    remaining(*id) & (1 << bit) == 0
                        || (self.records[*id].value & (1 << bit) != 0) == set
                })
                .collect()
        };
        let (ones, zeros) = (side(true), side(false));
        let one = self.node(&ones, tested | (1 << bit), depth + 1, false);
        let zero = self.node(&zeros, tested | (1 << bit), depth + 1, false);
        format!(
            "if w & {:#010x} != 0 {{\n{pad}    {one}\n{pad}}} else {{\n{pad}    {zero}\n{pad}}}",
            1u32 << bit
        )
    }
}

fn extraction(parts: &Parts) -> String {
    let terms: Vec<String> = parts
        .iter()
        .map(|(word, field, len)| {
            let mask = (1u64 << len) - 1;
            let mut term = if *word == 0 {
                format!("(w & {mask:#x})")
            } else {
                format!("((w >> {word}) & {mask:#x})")
            };
            if *field != 0 {
                term = format!("({term} << {field})");
            }
            term
        })
        .collect();
    terms.join(" | ")
}

pub fn generate(json: &Value, config: &Config, sha256: &str) -> Result<Output> {
    let names = json["!instanceof"]["EncodedI"]
        .as_array()
        .context("the dump has no EncodedI records")?;
    let mut records = Vec::new();
    let mut unselected = 0;
    for name in names {
        let name = name.as_str().context("record name")?;
        if let Some(record) =
            parse_record(json, name, config, &mut unselected).with_context(|| name.to_string())?
        {
            records.push(record);
        }
    }
    records.sort_by(|a, b| a.name.cmp(&b.name));
    if records.len() > usize::from(u16::MAX) {
        bail!("more records than a u16 record id holds");
    }

    // Constraints (G1d).
    let mut used = vec![false; config.constraints.len()];
    for record in &mut records {
        let mut conditions = Vec::new();
        for (id, constraint) in config.constraints.iter().enumerate() {
            if constraint.format != record.format
                || !(constraint.records.is_empty() || constraint.records.contains(&record.name))
            {
                continue;
            }
            used[id] = true;
            conditions.push(format!("({})", constraint.require));
        }
        if !conditions.is_empty() {
            record.constraint = Some(conditions.join(" && "));
        }
    }
    for (id, constraint) in config.constraints.iter().enumerate() {
        if !used[id] {
            bail!(
                "constraints.toml: the constraint on {} {:?} matches no selected record",
                constraint.format,
                constraint.records
            );
        }
        for name in &constraint.records {
            if !records
                .iter()
                .any(|record| &record.name == name && record.format == constraint.format)
            {
                bail!(
                    "constraints.toml: {name} is not a selected record of {}",
                    constraint.format
                );
            }
        }
    }

    let order = prioritise(&mut records, config)?;

    // Format classes and their fields.
    let mut formats: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for record in &records {
        formats
            .entry(record.format.clone())
            .or_default()
            .extend(record.fields.keys().cloned());
    }
    for record in &records {
        if formats.contains_key(&record.name) && record.format != record.name {
            bail!("the record {} has the name of a format class", record.name);
        }
    }

    // Layouts: records of one class that share field positions and constraint.
    let mut layouts: Vec<(String, BTreeMap<String, Parts>, Option<String>)> = Vec::new();
    let mut layout_ids = BTreeMap::new();
    for record in &mut records {
        let key = (
            record.format.clone(),
            record.fields.clone(),
            record.constraint.clone(),
        );
        record.layout = *layout_ids.entry(key.clone()).or_insert_with(|| {
            layouts.push(key);
            layouts.len() - 1
        });
    }

    let mut out = String::from(crate::NOTICE);
    out.push_str(
        "//\n// Input: the JSON dump of AArch64.td by the pinned llvm-tblgen\n\
         // (hv/toolchain/llvm/pins.env), and tables/{sets,overrides,constraints}.toml.\n\n\
         #![allow(non_camel_case_types, unused_parens, clippy::all)]\n\n",
    );
    let _ = writeln!(
        out,
        "/// sha256 of the JSON dump that this file was generated from.\n\
         pub const TABLES_SHA256: &str = {sha256:?};\n"
    );
    out.push_str(
        "/// The features of the selected sets (tables/sets.toml) as a bit set.\n\
         #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]\n\
         pub struct Features(pub u16);\n\nimpl Features {\n    pub const BASE: Self = Self(0);\n",
    );
    for (index, feature) in config.features.iter().enumerate() {
        let _ = writeln!(
            out,
            "    pub const {}: Self = Self({:#x});",
            feature.to_ascii_uppercase(),
            1u16 << index
        );
    }
    out.push_str("}\n\n/// The LLVM names of the selected features, in bit order.\n");
    let _ = writeln!(
        out,
        "pub const FEATURE_NAMES: [&str; {}] = {:?};\n",
        config.features.len(),
        config.features
    );
    let mut attributes = Vec::new();
    for feature in &config.features {
        let record = &json[format!("Feature{feature}")];
        attributes.push(
            record["Name"]
                .as_str()
                .with_context(|| format!("sets.toml: LLVM has no Feature{feature}"))?
                .to_string(),
        );
    }
    let _ = writeln!(
        out,
        "/// The same features as `-mattr` of the LLVM tools spells them.\n\
         pub const FEATURE_ATTRIBUTES: [&str; {}] = {:?};\n",
        attributes.len(),
        attributes
    );

    out.push_str(
        "/// One part of a field: `len` bits of the word from `word_lsb` are the\n\
         /// bits of the field from `field_lsb`.\n\
         #[derive(Clone, Copy, Debug, PartialEq, Eq)]\n\
         pub struct FieldPart {\n    pub word_lsb: u8,\n    pub field_lsb: u8,\n    pub len: u8,\n}\n\n\
         #[derive(Clone, Copy, Debug, PartialEq, Eq)]\n\
         pub struct FieldInfo {\n    pub name: &'static str,\n    pub parts: &'static [FieldPart],\n}\n\n\
         #[derive(Clone, Copy, Debug)]\n\
         pub struct RecordInfo {\n    /// The name of the LLVM record.\n    pub name: &'static str,\n    \
         pub mask: u32,\n    pub value: u32,\n    /// The selected features that its predicates name.\n    \
         pub features: Features,\n    pub format: FormatClass,\n    \
         /// A record of LLVM's `Fallback` decoder namespace.\n    pub fallback: bool,\n    \
         /// Its place in the order in which the decoder tries records.\n    pub priority: u16,\n    \
         /// Whether tables/constraints.toml adds a condition to it.\n    pub constrained: bool,\n    \
         pub fields: &'static [FieldInfo],\n    layout: u16,\n}\n\n",
    );

    // Record enum.
    out.push_str(
        "/// One record of the LLVM tables, by its LLVM name.\n\
         #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]\n#[repr(u16)]\npub enum Record {\n",
    );
    for record in &records {
        let _ = writeln!(out, "    {},", record.name);
    }
    out.push_str("}\n\n");
    let _ = writeln!(
        out,
        "impl Record {{\n    pub const COUNT: usize = {};\n\n    \
         pub fn info(self) -> &'static RecordInfo {{\n        &RECORDS[self as usize]\n    }}\n\n    \
         pub fn name(self) -> &'static str {{\n        self.info().name\n    }}\n\n    \
         pub fn from_index(index: usize) -> Option<Self> {{\n        Self::ALL.get(index).copied()\n    }}\n\n    \
         pub const ALL: [Self; {}] = [",
        records.len(),
        records.len()
    );
    for record in &records {
        let _ = writeln!(out, "        Self::{},", record.name);
    }
    out.push_str("    ];\n}\n\n");

    // Format class enum.
    out.push_str(
        "/// A format class: the last superclass of a record in the LLVM tables.\n\
         #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]\n#[repr(u16)]\npub enum FormatClass {\n",
    );
    for format in formats.keys() {
        let _ = writeln!(out, "    {format},");
    }
    out.push_str("}\n\n");
    let _ = writeln!(
        out,
        "impl FormatClass {{\n    pub const COUNT: usize = {};\n\n    pub const ALL: [Self; {}] = [",
        formats.len(),
        formats.len()
    );
    for format in formats.keys() {
        let _ = writeln!(out, "        Self::{format},");
    }
    out.push_str("    ];\n\n    pub fn name(self) -> &'static str {\n        match self {\n");
    for format in formats.keys() {
        let _ = writeln!(out, "            Self::{format} => {format:?},");
    }
    out.push_str("        }\n    }\n}\n\n");

    // Field structs.
    for (format, fields) in &formats {
        let _ = writeln!(
            out,
            "/// The raw fields of the format class `{format}`.\n\
             #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]\npub struct {format} {{"
        );
        for field in fields {
            let _ = writeln!(out, "    pub {field}: u32,");
        }
        out.push_str("}\n\n");
    }
    out.push_str(
        "/// The raw fields of a decoded instruction, by format class.\n\
         #[derive(Clone, Copy, Debug, PartialEq, Eq)]\npub enum Fields {\n",
    );
    for format in formats.keys() {
        let _ = writeln!(out, "    {format}({format}),");
    }
    out.push_str("}\n\n");
    out.push_str(
        "#[derive(Clone, Copy, Debug, PartialEq, Eq)]\npub struct Decoded {\n    \
         pub record: Record,\n    pub fields: Fields,\n}\n\n",
    );

    // Field layouts.
    for (id, (_, fields, _)) in layouts.iter().enumerate() {
        let _ = write!(out, "static L{id}: [FieldInfo; {}] = [", fields.len());
        for (name, parts) in fields {
            let _ = write!(out, "\n    FieldInfo {{ name: {name:?}, parts: &[");
            for (word_lsb, field_lsb, len) in parts {
                let _ = write!(
                    out,
                    "FieldPart {{ word_lsb: {word_lsb}, field_lsb: {field_lsb}, len: {len} }}, "
                );
            }
            out.push_str("] },");
        }
        out.push_str(if fields.is_empty() { "];\n" } else { "\n];\n" });
    }
    out.push('\n');

    // Record table.
    let _ = writeln!(
        out,
        "/// Every selected record, in the order of `Record`.\npub static RECORDS: [RecordInfo; {}] = [",
        records.len()
    );
    for record in &records {
        let _ = writeln!(
            out,
            "    RecordInfo {{ name: {:?}, mask: {:#010x}, value: {:#010x}, features: Features({:#x}), \
             format: FormatClass::{}, fallback: {}, priority: {}, constrained: {}, fields: &L{}, layout: {} }},",
            record.name,
            record.mask,
            record.value,
            record.features,
            record.format,
            record.fallback,
            record.priority,
            record.constraint.is_some(),
            record.layout,
            record.layout
        );
    }
    out.push_str("];\n\n");

    // Extraction.
    out.push_str(
        "/// The raw fields of `w` as the record reads them, or `None` when a\n\
         /// constraint of tables/constraints.toml does not hold. It does not test\n\
         /// the fixed bits of the record.\n\
         pub fn extract(record: Record, w: u32) -> Option<Fields> {\n    \
         Some(match RECORDS[record as usize].layout {\n",
    );
    for (id, (format, fields, constraint)) in layouts.iter().enumerate() {
        let _ = writeln!(out, "        {id} => {{");
        for (name, parts) in fields {
            let _ = writeln!(out, "            let {name} = {};", extraction(parts));
        }
        if let Some(constraint) = constraint {
            let _ = writeln!(
                out,
                "            if !({constraint}) {{\n                return None;\n            }}"
            );
        }
        let all = &formats[format];
        let mut initialiser = fields.keys().cloned().collect::<Vec<_>>().join(", ");
        if all.len() != fields.len() {
            if !initialiser.is_empty() {
                initialiser.push_str(", ");
            }
            initialiser.push_str("..Default::default()");
        }
        let _ = writeln!(
            out,
            "            Fields::{format}({format} {{ {initialiser} }})\n        }}"
        );
    }
    out.push_str(
        "        _ => unreachable!(\"a layout id from the generated table\"),\n    })\n}\n\n\
         #[inline]\nfn leaf(record: Record, w: u32) -> Option<Decoded> {\n    \
         extract(record, w).map(|fields| Decoded { record, fields })\n}\n\n",
    );

    // The decision tree.
    let mut tree = Tree {
        records: &records,
        functions: Vec::new(),
    };
    let root = tree.node(&order, 0, 1, true);
    let _ = writeln!(
        out,
        "/// Decode one A64 word. `None` is an encoding that no selected record\n\
         /// matches: it takes the Undefined Instruction exception.\n\
         pub fn decode(w: u32) -> Option<Decoded> {{\n    {root}\n}}\n"
    );
    for function in &tree.functions {
        out.push_str(function);
        out.push('\n');
    }
    while out.ends_with("\n\n") {
        out.pop();
    }

    Ok(Output {
        source: out,
        records: records.len(),
        formats: formats.len(),
        constraints: config.constraints.len(),
    })
}
