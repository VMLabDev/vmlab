//! Prerequisites for tests that send PCM through the host's audio session.

use serde_json::Value;
use std::io::Write;
use std::process::Command;

pub(super) fn available(test: &str, pipewire: bool) -> bool {
    let (program, args): (&str, &[&str]) = if pipewire {
        ("pw-dump", &[])
    } else {
        ("pactl", &["--format=json", "list", "sinks"])
    };
    let result = Command::new(program)
        .args(args)
        .output()
        .map_err(|e| format!("cannot inspect audio sinks with {program}: {e}"))
        .and_then(|output| {
            if !output.status.success() {
                return Err(format!("{program} cannot reach the audio session"));
            }
            let value = serde_json::from_slice(&output.stdout)
                .map_err(|e| format!("invalid {program} sink report: {e}"))?;
            usable_sink(&value, pipewire)
        });
    match result {
        Ok(()) => true,
        Err(reason) => {
            assert!(
                std::env::var_os("VMLAB_REQUIRE_AUDIO").is_none_or(|v| v != "1"),
                "{test}: {reason} (VMLAB_REQUIRE_AUDIO=1)"
            );
            // Write directly so successful libtest output capture cannot hide
            // a missing prerequisite in the release gate log.
            let _ = writeln!(std::io::stderr(), "SKIP {test}: {reason}");
            false
        }
    }
}

fn usable_sink(value: &Value, pipewire: bool) -> Result<(), String> {
    let entries = value.as_array().ok_or("sink report is not an array")?;
    let mut dummy = false;
    for entry in entries {
        let properties = if pipewire {
            let props = &entry["info"]["props"];
            if props["media.class"] != "Audio/Sink" {
                continue;
            }
            props
        } else {
            entry
        };
        let name = if pipewire {
            properties["node.name"].as_str()
        } else {
            properties["name"].as_str()
        };
        match name {
            Some("auto_null") => dummy = true,
            Some(_) => return Ok(()),
            None => return Err("audio sink has no name".into()),
        }
    }
    Err(if dummy {
        "only the auto_null Dummy Output sink is available, with no usable driven sink".into()
    } else {
        "audio session has no playback sink".into()
    })
}

#[test]
fn sink_prerequisites_distinguish_headless_and_desktop_sessions() {
    use serde_json::json;
    for pipewire in [false, true] {
        let sink = |name| {
            if pipewire {
                json!({"info": {"props": {"media.class": "Audio/Sink", "node.name": name}}})
            } else {
                json!({"name": name})
            }
        };
        assert!(
            usable_sink(&json!([]), pipewire)
                .unwrap_err()
                .contains("no playback sink")
        );
        assert!(
            usable_sink(&json!([sink("auto_null")]), pipewire)
                .unwrap_err()
                .contains("auto_null")
        );
        assert!(
            usable_sink(
                &json!([sink("auto_null"), sink("alsa_output.test")]),
                pipewire
            )
            .is_ok()
        );
        assert!(usable_sink(&json!([sink("test-driven-null")]), pipewire).is_ok());
        assert!(usable_sink(&Value::Null, pipewire).is_err());
    }
    assert!(
        usable_sink(
            &json!([{"info": {"props": {"media.class": "Audio/Source", "node.name": "mic"}}}]),
            true
        )
        .is_err()
    );
}
