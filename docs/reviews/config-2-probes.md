# Probes for the second config review

These are the temporary tests behind `docs/reviews/config-2.md`. They are not
in the tree. Each one **passes while its fault is present**, so a fix card
turns the final assertions round and keeps the rest. Some only print: what
they printed is at the end.

Each block was pasted at the end of `mod tests` of the file its heading
names, on commit `759d5fa7`, in the checkout of card #968 on helios, and run
with:

```
cargo test --all-features --lib review968 -- --nocapture --test-threads=1
```

Result: 13 passed, 0 failed, in two runs (11 in the first, then
`review968_p8b` and `review968_p8c` alone). The checkout was put back with
`git apply -R` after each run. The names carry the probe numbers of the run,
which are not the finding codes. The map is:

| Probe | Finding or note |
|---|---|
| `review968_p8_…`, `review968_p8b_…`, `review968_p8c_…` | F18 |
| `review968_p1_…` | F19 |
| `review968_p2_…` | F20 |
| `review968_p6_…` | F21, F22, F26, N28 |
| `review968_p4_…` | F23 |
| `review968_p3_…` | F24 |
| `review968_p5_…` | F25 |
| `review968_p7b_…` | F27 |
| `review968_p10_…` | F28 |
| `review968_p11_…` | F29, N22, N23, N24 |
| `review968_p7_…` | N21, and the check that no host error quotes the PSK |

F17 has no probe.

`review968_p8b` catches the panic with `catch_unwind` to show it. That is a
probe, not a test to keep: the test proposed for F18 expects an error.

## `src/config/validate.rs`

```rust
    // ---- review968 probes: each passes while its fault is present ----
    fn review968_errs(root: &Path, src: &str) -> Vec<String> {
        let file = load_lab_source(src, "<test>", root).expect("loads");
        validate(&file, &Permissive)
            .into_iter()
            .map(|i| i.message)
            .collect()
    }

    #[test]
    fn review968_p1_a_tilde_share_is_judged_under_the_lab_root() {
        let home = crate::paths::home();
        assert!(home.is_dir(), "{}", home.display());
        let root = tempfile::tempdir().unwrap();
        let src = "import <vmlab.wcl>\nlab \"l\" {\n segment \"s\" { subnet = \"10.1.1.0/24\" }\n vm \"a\" { template = \"x86_64/t\" nic { segment = \"s\" } share { host = \"~\" guest = \"/mnt/h\" } }\n}";
        let es = review968_errs(root.path(), src);
        eprintln!("p1 before: {es:#?}");
        assert!(es.iter().any(|m| m.contains("share host path ~ is not a directory")));
        let served = crate::labd::share_plan::resolve_share_host(root.path(), Some(&home), Path::new("~"));
        assert_eq!(served, home);
        std::fs::create_dir(root.path().join("~")).unwrap();
        let es = review968_errs(root.path(), src);
        eprintln!("p1 after: {es:#?}");
        assert!(!es.iter().any(|m| m.contains("is not a directory")));
    }

    #[test]
    fn review968_p2_a_guest_root_spelled_with_dots_passes() {
        let root = tempfile::tempdir().unwrap();
        for (guest, machine) in [
            ("/.", r#"vm "d" { template = "x86_64/t" profile = "linux" }"#),
            ("/..", r#"vm "d" { template = "x86_64/t" profile = "linux" }"#),
            ("/tmp/..", r#"container "d" { image = "sdk:9.0" }"#),
            ("C:\\\\.", r#"vm "d" { template = "x86_64/t" profile = "windows-11" }"#),
            ("C:\\\\Users\\\\..", r#"vm "d" { template = "x86_64/t" profile = "windows-11" }"#),
        ] {
            let src = format!("import <vmlab.wcl>\nlab \"l\" {{\n@dev(workspace_guest = \"{guest}\") {machine}\n}}");
            let es = review968_errs(root.path(), &src);
            assert!(es.is_empty(), "{guest}: {es:#?}");
        }
    }

    #[test]
    fn review968_p3_a_relative_share_guest_path_passes() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("d")).unwrap();
        for (guest, profile) in [("mnt/src", "linux"), ("", "linux"), ("data", "windows-11"), ("/mnt/x", "windows-11")] {
            let src = format!("import <vmlab.wcl>\nlab \"l\" {{\n segment \"s\" {{ subnet = \"10.1.1.0/24\" }}\n vm \"a\" {{ template = \"x86_64/t\" profile = \"{profile}\" nic {{ segment = \"s\" }} share {{ host = \"d\" guest = \"{guest}\" name = \"n\" }} }}\n}}");
            let es = review968_errs(root.path(), &src);
            assert!(es.is_empty(), "{guest}: {es:#?}");
        }
    }

    #[test]
    fn review968_p4_a_mac_no_nic_can_hold_passes() {
        let root = tempfile::tempdir().unwrap();
        for mac in ["01:00:5e:00:00:01", "00:00:00:00:00:00", "ff:ff:ff:ff:ff:ff", "+1:+2:+3:+4:+5:+6"] {
            let src = format!("import <vmlab.wcl>\nlab \"l\" {{\n segment \"s\" {{ subnet = \"10.1.1.0/24\" }}\n vm \"a\" {{ template = \"x86_64/t\" nic {{ segment = \"s\" mac = \"{mac}\" }} }}\n}}");
            let es = review968_errs(root.path(), &src);
            assert!(es.is_empty(), "{mac}: {es:#?}");
        }
    }

    #[test]
    fn review968_p5_a_registry_reference_the_pull_refuses_passes() {
        let root = tempfile::tempdir().unwrap();
        for reference in ["ghcr.io/a/../b:1", "ghcr.io/a b/c:1", "ghcr.io//x:1"] {
            let parsed = crate::oci::reference::Reference::parse(reference);
            eprintln!("p5 {reference}: parse = {:?}", parsed.as_ref().map(|_| ()).map_err(|e| e.to_string()));
            let src = format!("import <vmlab.wcl>\nlab \"l\" {{\n vm \"a\" {{ template = \"{reference}\" arch = \"x86_64\" }}\n}}");
            let es = review968_errs(root.path(), &src);
            eprintln!("p5 {reference}: validate = {es:#?}");
        }
        assert!(crate::oci::reference::Reference::parse("ghcr.io/a/../b:1").is_err());
        let src = "import <vmlab.wcl>\nlab \"l\" {\n vm \"a\" { template = \"ghcr.io/a/../b:1\" arch = \"x86_64\" }\n}";
        assert!(review968_errs(root.path(), src).is_empty());
    }

    #[test]
    fn review968_p11_misc_prints() {
        let root = tempfile::tempdir().unwrap();
        for body in [
            "segment \"s\" { subnet = \"10.1.1.0/24\" forward { host_port = 5353 to = \"a:53\" proto = \"tcp\" } forward { host_port = 5353 to = \"a:5353\" proto = \"udp\" } }\n vm \"a\" { template = \"x86_64/t\" nic { segment = \"s\" } }",
            "segment \"s\" { subnet = \"10.1.1.0/24\" }\n container \"c\" { image = \"x:1\" nic { segment = \"s\" } volume { name = \"a\" target = \"/\" } volume { name = \"b\" target = \"/data\" } volume { name = \"c\" target = \"/data\" } env { name = \"A\" value = \"1\" } env { name = \"A\" value = \"2\" } }",
            "segment \"s\" { subnet = \"10.1.1.5/24\" dns { server = \"192.168.9.9\" } }\n vm \"a\" { template = \"x86_64/t\" memory = 0 nic { segment = \"s\" } disk \"../x\" { size = 0 } }",
            "segment \"s\" { subnet = \"10.1.1.0/24\" dns { server = \"10.1.1.9\" } dns { enabled = false } }",
        ] {
            let src = format!("import <vmlab.wcl>\nlab \"l\" {{\n {body}\n}}");
            match load_lab_source(&src, "<test>", root.path()) {
                Ok(file) => {
                    let es: Vec<String> = validate(&file, &Permissive).into_iter().map(|i| i.message).collect();
                    eprintln!("p11 [{body}]\n -> validate {es:#?}");
                }
                Err(e) => eprintln!("p11 [{body}]\n -> load {:?}", e.issues.iter().map(|i| &i.message).collect::<Vec<_>>()),
            }
        }
    }
```

## `src/config/host.rs`

```rust
    // ---- review968 probes ----
    #[test]
    fn review968_p6_host_values_nothing_can_use_load() {
        for body in [
            "host { workspace_max_file = 0 }\n",
            "host { subnet_pool = \"10.0.0.0/28\" }\n",
            "host { subnet_pool = \"0.0.0.0/0\" }\n",
            "host { subnet_pool = \"224.0.0.0/4\" }\n",
            "host { dns_upstream = \"10.0.0.1:0\" }\n",
            "host { dns_upstream = \"0.0.0.0\" }\n",
            "host { dns_suffix = \"\" }\n",
            "host { dns_suffix = \"has space\" }\n",
            "host { dns_suffix = \".lead.dot.\" }\n",
            "host { trusted_template_repositories = [\"\", \"/\", \"ghcr.io\"] }\n",
        ] {
            let source = format!("import <vmlab-host.wcl>\n{body}");
            let cfg = HostConfig::parse(&source, "config.wcl");
            assert!(cfg.is_ok(), "{body}: {:#}", cfg.unwrap_err());
        }
        let pool: ipnet::Ipv4Net = "10.0.0.0/28".parse().unwrap();
        assert_eq!(pool.subnets(24).into_iter().flatten().count(), 0);
        let all: ipnet::Ipv4Net = "0.0.0.0/0".parse().unwrap();
        let first = all.subnets(24).into_iter().flatten().next().unwrap();
        assert_eq!(first.to_string(), "0.0.0.0/24");
    }

    #[test]
    fn review968_p7_what_a_host_error_quotes() {
        for body in [
            "host { psk = \"sekrit-psk\" + 1 }\n",
            "host { psk = \"sekrit-psk\n}\n",
            "host { psk = \"sekrit-psk\" \"x\" }\n",
            "host { psk = [\"sekrit-psk\"] }\n",
            "host { psk = 99887766 }\n",
            "host { psk = \"sekrit-psk\" bogus_field = 1 }\n",
            "host { psk = \"sekrit-psk\" }\nhost { psk = \"other\" }\n",
            "let k = \"sekrit-psk\"\nhost { psk = k + 1 }\n",
            "host { psk = \"sekrit-psk\" ?? }\n",
        ] {
            let source = format!("import <vmlab-host.wcl>\n{body}");
            match HostConfig::parse(&source, "config.wcl") {
                Ok(_) => eprintln!("p7 [{}] -> OK", body.escape_debug()),
                Err(e) => {
                    let text = format!("{e:#}");
                    eprintln!("p7 [{}] leak={} -> {text}", body.escape_debug(), text.contains("sekrit") || text.contains("99887766"));
                }
            }
        }
    }
```

## `src/config/mod.rs`

```rust
    // ---- review968 probes ----
    #[test]
    fn review968_p7b_what_a_lab_error_quotes() {
        let root = tempfile::tempdir().unwrap();
        for login in [
            "login \"d\" { user = \"u\" password = \"hunter2-pw\" + 1 }",
            "login \"d\" { user = \"u\" password = \"hunter2-pw }",
            "login \"d\" { user = \"u\" password = [\"hunter2-pw\"] }",
            "login \"d\" { user = \"u\" password = 99887766 }",
            "login \"d\" { user = \"u\" password = \"hunter2-pw\" ?? }",
            "login \"d\" { user = \"u\" password = \"hunter2-pw\" } playbook \"pb\" { play = \"p\" var \"t\" { value = 99887766 secret = true } }",
            "login \"d\" { user = \"u\" passwd = \"hunter2-pw\" }",
        ] {
            let src = format!("import <vmlab.wcl>\nlab \"l\" {{\n vm \"a\" {{ template = \"x86_64/t\" {login} }}\n}}\n");
            match load_lab_source(&src, "vmlab.wcl", root.path()) {
                Ok(_) => eprintln!("p7b [{login}] -> OK"),
                Err(e) => {
                    let text = format!("{:?}", miette::Report::new(e));
                    eprintln!("p7b [{login}] leak={} ->\n{text}", text.contains("hunter2") || text.contains("99887766"));
                }
            }
        }
    }

    #[test]
    fn review968_p8_an_issue_in_an_imported_file() {
        let root = tempfile::tempdir().unwrap();
        let pad = "// pad pad pad pad pad pad pad pad pad pad\n".repeat(40);
        std::fs::write(
            root.path().join("inc.wcl"),
            format!("import <vmlab.wcl>\n{pad}template \"t\" {{ arch = \"bogus\" version = \"1\" source \"scratch\" {{ }} login \"d\" {{ user = \"u\" password = \"hunter2-import\" }} }}\n"),
        )
        .unwrap();
        let src = "import <vmlab.wcl>\nimport \"inc.wcl\"\nlab \"l\" { vm \"a\" { template = \"x86_64/t\" login \"d\" { user = \"u\" password = \"hunter2-main\" } } }\n// \u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\n";
        match load_lab_source(src, "vmlab.wcl", root.path()) {
            Ok(f) => eprintln!("p8 OK templates={}", f.templates.len()),
            Err(e) => {
                eprintln!("p8 main source len {}", src.len());
                for i in &e.issues {
                    eprintln!("p8 issue span={:?} {}", i.span, i.message);
                }
                let text = format!("{:?}", miette::Report::new(e));
                eprintln!("p8 leak={} ->\n{text}", text.contains("hunter2"));
            }
        }
    }
```

## `src/instance.rs`

```rust
    // ---- review968 probes ----
    #[test]
    fn review968_p10_two_directories_one_effective_name() {
        assert_eq!(effective_name("demo", "fix_1").unwrap(), effective_name("demo", "fix-1").unwrap());
        assert_eq!(effective_name("demo", "Fix.1").unwrap(), "demo-fix-1");
        assert_eq!(effective_name("demo-a", "b").unwrap(), effective_name("demo", "a-b").unwrap());
        assert!(belongs_to("demo-a-b", "demo") && belongs_to("demo-a-b", "demo-a"));
    }
```

## `src/profiles/mod.rs`

```rust
    // ---- review968 probes ----
    #[test]
    fn review968_p8b_a_profile_issue_in_an_imported_file() {
        let dir = tempfile::tempdir().unwrap();
        let inc = dir.path().join("inc.wcl");
        std::fs::write(&inc, "import <vmlab-profile.wcl>\nprofile \"p\" { machine = \"bogus\" }\n").unwrap();
        let e = "\u{e9}".repeat(40);
        let main = format!("import <vmlab-profile.wcl>\n// {e}\nimport \"{}\"\nprofile \"q\" {{ }}\n", inc.display());
        let got = std::panic::catch_unwind(|| parse_profiles(&main, "main.wcl").map(|v| v.len()).map_err(|e| format!("{e:#}")));
        eprintln!("p8b -> {got:?}");
        assert!(got.is_err(), "expected the panic in line_col");
    }
```

## `src/config/host.rs`

```rust
    #[test]
    fn review968_p8c_a_host_issue_in_an_imported_file() {
        let dir = tempfile::tempdir().unwrap();
        let inc = dir.path().join("inc.wcl");
        std::fs::write(&inc, "import <vmlab-host.wcl>\n\n\n\n\n\n\n\ngpu \"g\" { address = \"0000:01:00.0\" keepawake = \"bogus\" }\n").unwrap();
        let main = format!("import <vmlab-host.wcl>\nimport \"{}\"\nhost {{ disk_low_percent = 10 }}\n", inc.display());
        let got = HostConfig::parse(&main, "config.wcl").map(|c| c.gpus.len()).map_err(|e| format!("{e:#}"));
        eprintln!("p8c main is 3 lines -> {got:?}");
        assert!(got.unwrap_err().contains("config.wcl:"));
    }
```

## What the probes printed

Compiler lines and the tests that print nothing are left out. The lab-file
reports of `p7b` are cut to the lines that matter.

```
p7 [host { psk = "sekrit-psk" + 1 }] leak=false -> 1 error(s) in config.wcl
  config.wcl:2:8: cannot evaluate `psk`: operator '+' is not defined for utf8 and i64
p7 [host { psk = "sekrit-psk<newline>}] leak=false -> parse error in config.wcl: newline in string literal
p7 [host { psk = "sekrit-psk" "x" }] leak=false -> parse error in config.wcl: expected identifier, found string
p7 [host { psk = ["sekrit-psk"] }] leak=false -> 2 error(s) in config.wcl
  config.wcl:2:8: field 'psk' declared as utf8 but value is list
  config.wcl:2:8: `psk` must be a string, got a list
p7 [host { psk = 99887766 }] leak=false -> 2 error(s) in config.wcl
  config.wcl:2:8: field 'psk' declared as utf8 but value is i64
  config.wcl:2:8: `psk` must be a string, got an integer
p7 [host { psk = "sekrit-psk" bogus_field = 1 }] leak=false -> 1 error(s) in config.wcl
  config.wcl:2:27: field 'bogus_field' is not declared by schema 'HostConfig'
p7 [host { psk = "sekrit-psk" }<newline>host { psk = "other" }] -> OK
p7 [let k = "sekrit-psk"<newline>host { psk = k + 1 }] leak=false -> 1 error(s) in config.wcl
  config.wcl:3:8: cannot evaluate `psk`: operator '+' is not defined for utf8 and i64
p7 [host { psk = "sekrit-psk" ?? }] leak=false -> parse error in config.wcl: expected value, found '}'

p7b [login "d" { user = "u" password = "hunter2-pw" + 1 }] leak=false
  cannot evaluate `password`: operator '+' is not defined for utf8 and i64
  3 |  vm "a" { template = "x86_64/t" login "d" { user = "u" password = **************** } }
p7b [login "d" { user = "u" password = "hunter2-pw }] leak=false
  parse error: newline in string literal
p7b [login "d" { user = "u" password = ["hunter2-pw"] }] leak=false
  field 'password' declared as utf8 but value is list
  3 |  vm "a" { template = "x86_64/t" login "d" { user = "u" password = ************** } }
p7b [login "d" { user = "u" password = 99887766 }] leak=false
  3 |  vm "a" { template = "x86_64/t" login "d" { user = "u" password = ******** } }
p7b [login "d" { user = "u" password = "hunter2-pw" ?? }] leak=false
  parse error: expected value, found '}'
p7b [… var "t" { value = 99887766 secret = true } …] leak=false
  3 |  … password = ************ } playbook "pb" { play = "p" var "t" { value = ******** secret = true } } }
p7b [login "d" { user = "u" passwd = "hunter2-pw" }] leak=true
  field 'passwd' is not declared by schema 'Login'
  3 |  vm "a" { template = "x86_64/t" login "d" { user = "u" passwd = "hunter2-pw" } }

p8 main source len 176
p8 issue span=Some(SourceSpan { offset: SourceOffset(1754), length: 14 }) `arch` must be one of x86_64, x86, aarch64, riscv64, loongarch64, s390x, ppc64, got `bogus`
p8 leak=false ->
  1 error(s) in vmlab.wcl
  `arch` must be one of x86_64, x86, aarch64, riscv64, loongarch64, s390x, ppc64, got `bogus`
  [Failed to read contents for label `<none>` (offset: 1754, length: 14): OutOfBounds]

p8b thread … panicked at src/config/block.rs:695:23:
    end byte index 41 is not a char boundary; it is inside 'é' (bytes 40..42 of string)
p8b -> Err(Any { .. })
p8c main is 3 lines -> Err("1 error(s) in config.wcl\n  config.wcl:3:10: `keepawake` must be one of auto, on, off, got `bogus`")

p11 [tcp and udp forward on host_port 5353] -> validate ["duplicate forward host_port 5353"]
p11 [volumes with target "/", "/data", "/data"; env "A" twice] -> validate []
p11 [subnet = "10.1.1.5/24", dns server 192.168.9.9, memory = 0, disk "../x" { size = 0 }] -> validate []
p11 [two dns blocks in one segment] -> load ["field 'dns' expects a single 'dns' child, found 2"]

p1 before: ["share host path ~ is not a directory"]
p1 after: []

p5 ghcr.io/a/../b:1: parse = Err("reference `ghcr.io/a/../b:1` has an invalid repository `a/../b`: each `/`-separated segment is letters, digits, `.`, `_` and `-`, and is not `.` or `..`")
p5 ghcr.io/a/../b:1: validate = []
p5 ghcr.io/a b/c:1: parse = Err(… invalid repository `a b/c` …)
p5 ghcr.io/a b/c:1: validate = []
p5 ghcr.io//x:1: parse = Err(… invalid repository `/x` …)
p5 ghcr.io//x:1: validate = []

test result: ok. 11 passed; 0 failed; 0 ignored; 0 measured; 2860 filtered out
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 2860 filtered out
```
