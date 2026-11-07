//! S1, VML-107: guest writes survive a SIGTERM stop.
//!
//! A ROM-only `pc` machine (no guest assets) writes sectors to new clusters
//! of an IDE qcow2 disk by ATA PIO. The IDE write cache is on by default and
//! the ROM never sends FLUSH CACHE, so the L2 and refcount updates for those
//! clusters are only in vmlab-hv's metadata caches when the test sends
//! SIGTERM. After the process exits, `qemu-img check` must be clean and the
//! sectors must read back.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use vmlab_hv_tests::process::ManagedChild;

/// 2 GiB: with 64 KiB clusters one L2 table maps 512 MiB, so the writes at
/// 1 GiB and at the last sector need new L2 tables and an L1 update.
const DISK_SIZE: u64 = 2 << 30;

/// Sectors the ROM writes, each filled with its own 16-bit word.
const WRITES: [(u32, u16); 5] = [
    (0, 0xa501),
    (128, 0xa502),
    (129, 0xa503),
    (0x20_0000, 0xa504),
    ((DISK_SIZE / 512 - 1) as u32, 0xa505),
];

// The ROM uses no BIOS services. It enables the PIIX3 IDE function's I/O
// decode, then writes one sector per WRITE SECTORS (0x30) command and polls
// the status register. COM1 carries the result to the serial log.
const ROM_HEAD: &str = r#"
.section .text
.code16
.global _start
_start:
    cli
    cld
    xorw %ax, %ax
    movw %ax, %ds
    movw %ax, %ss
    movw $0x8000, %sp

    # 00:01.1 command register: I/O space on.
    movw $0xcf8, %dx
    movl $0x80000904, %eax
    outl %eax, %dx
    movw $0xcfc, %dx
    movl $1, %eax
    outl %eax, %dx
    # IDETIM: primary and secondary decode on.
    movw $0xcf8, %dx
    movl $0x80000940, %eax
    outl %eax, %dx
    movw $0xcfc, %dx
    movl $0x80008000, %eax
    outl %eax, %dx
    # nIEN: the ROM polls, IRQ14 stays low.
    movw $0x3f6, %dx
    movb $2, %al
    outb %al, %dx

    movw $(table - _start), %si
next:
    cs lodsl
    cmpl $0xffffffff, %eax
    je done
    movl %eax, %ebx
    cs lodsw
    movw %ax, %bp
    call wait_ready
    movw $0x1f6, %dx
    movl %ebx, %eax
    shrl $24, %eax
    andb $0x0f, %al
    orb $0xe0, %al
    outb %al, %dx
    movw $0x1f2, %dx
    movb $1, %al
    outb %al, %dx
    movw $0x1f3, %dx
    movb %bl, %al
    outb %al, %dx
    movw $0x1f4, %dx
    movb %bh, %al
    outb %al, %dx
    movw $0x1f5, %dx
    movl %ebx, %eax
    shrl $16, %eax
    outb %al, %dx
    movw $0x1f7, %dx
    movb $0x30, %al
    outb %al, %dx
1:  inb %dx, %al
    testb $1, %al
    jnz failed
    testb $8, %al
    jz 1b
    movw $0x1f0, %dx
    movw $256, %cx
    movw %bp, %ax
2:  outw %ax, %dx
    loop 2b
    call wait_ready
    movw $0x1f7, %dx
    inb %dx, %al
    testb $0x29, %al
    jnz failed
    jmp next

done:
    movw $(ok - _start), %si
    jmp print
failed:
    movw $(bad - _start), %si
print:
    movw $0x3f8, %dx
1:  cs lodsb
    testb %al, %al
    jz 2f
    outb %al, %dx
    jmp 1b
2:  hlt
    jmp 2b

wait_ready:
    movw $0x1f7, %dx
1:  inb %dx, %al
    testb $0x80, %al
    jnz 1b
    ret

ok:  .asciz "VML107:WRITTEN\n"
bad: .asciz "VML107:FAILED\n"

.balign 4
table:
"#;

const ROM_TAIL: &str = r#"
    .long 0xffffffff

.org 0xfff0
    ljmp $0xf000, $0
    .fill 11, 1, 0xff
"#;

fn tool_available(tool: &str) -> bool {
    Command::new(tool)
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

fn build_rom(directory: &Path) -> PathBuf {
    let mut source = ROM_HEAD.to_string();
    for (lba, word) in WRITES {
        source.push_str(&format!("    .long {lba}\n    .word {word:#x}\n"));
    }
    source.push_str(ROM_TAIL);
    let source_path = directory.join("rom.S");
    let object = directory.join("rom.o");
    let linker = directory.join("rom.ld");
    let rom = directory.join("rom.bin");
    std::fs::write(&source_path, source).unwrap();
    std::fs::write(
        &linker,
        "SECTIONS { . = 0xf0000; .text : { *(.text) } /DISCARD/ : { *(*) } }\n",
    )
    .unwrap();
    let steps: [(&str, Vec<&std::ffi::OsStr>); 2] = [
        (
            "as",
            vec![
                "--32".as_ref(),
                "-o".as_ref(),
                object.as_os_str(),
                source_path.as_os_str(),
            ],
        ),
        (
            "ld",
            vec![
                "-m".as_ref(),
                "elf_i386".as_ref(),
                "-T".as_ref(),
                linker.as_os_str(),
                "--oformat".as_ref(),
                "binary".as_ref(),
                "-o".as_ref(),
                rom.as_os_str(),
                object.as_os_str(),
            ],
        ),
    ];
    for (program, arguments) in steps {
        let output = Command::new(program).args(arguments).output().unwrap();
        assert!(
            output.status.success(),
            "{program}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let bytes = std::fs::read(&rom).unwrap();
    assert_eq!(bytes.len(), 65536);
    assert_eq!(&bytes[0xfff0..0xfff5], &[0xea, 0, 0, 0, 0xf0]);
    rom
}

fn qemu_img(args: &[&std::ffi::OsStr]) -> String {
    let output = Command::new("qemu-img").args(args).output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        output.status.success(),
        "qemu-img {args:?} exited {}: {stdout}{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    stdout
}

#[test]
#[cfg_attr(vmlab_ci_no_kvm, ignore = "requires KVM and/or local guest assets")]
fn sigterm_keeps_guest_writes_to_new_qcow2_clusters() {
    if !Path::new("/dev/kvm").exists() {
        eprintln!("SKIP disk stop: /dev/kvm is unavailable");
        return;
    }
    for tool in ["as", "ld", "qemu-img"] {
        if !tool_available(tool) {
            eprintln!("SKIP disk stop: {tool} is unavailable");
            return;
        }
    }
    let temporary_root = std::env::var_os("TMPDIR").unwrap_or_else(|| "/var/tmp".into());
    let directory = tempfile::Builder::new()
        .prefix("hv-disk-stop-")
        .tempdir_in(temporary_root)
        .unwrap();
    let path = directory.path();
    let rom = build_rom(path);
    let disk = path.join("disk.qcow2");
    vmlab_disk::ops::create_qcow2(&disk, DISK_SIZE, None, None).unwrap();
    let serial = path.join("serial.log");
    let control = path.join("control.sock");
    let machine = path.join("machine.json");
    std::fs::write(
        &machine,
        serde_json::to_vec(&serde_json::json!({
            "name": "disk-stop", "machine": "pc", "arch": "x86_64", "accel": "kvm",
            "vcpus": 1, "memory_mib": 16,
            "boot": {"kind": "firmware", "firmware": "seabios", "code": rom},
            "disks": [{"id": "disk0", "bus": "ide", "channel": 0, "unit": 0,
                "path": disk, "format": "qcow2", "read_only": false, "vmstate": true}],
            "nics": [], "ports": [], "serial_log": serial,
        }))
        .unwrap(),
    )
    .unwrap();
    let stderr = path.join("stderr.log");
    let mut child = ManagedChild::spawn(
        Command::new("systemd-run")
            .args([
                "--user",
                "--scope",
                "-q",
                "-p",
                "MemoryMax=24G",
                "-p",
                "MemorySwapMax=0",
                "--",
            ])
            .arg(env!("CARGO_BIN_EXE_vmlab-hv"))
            .arg("--machine")
            .arg(&machine)
            .arg("--control")
            .arg(&control)
            .env("TMPDIR", path)
            .stdout(Stdio::null())
            .stderr(Stdio::from(std::fs::File::create(&stderr).unwrap())),
    )
    .unwrap();
    let logs = || {
        format!(
            "serial: {}\nstderr: {}",
            std::fs::read_to_string(&serial).unwrap_or_default(),
            std::fs::read_to_string(&stderr).unwrap_or_default()
        )
    };

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let text = std::fs::read_to_string(&serial).unwrap_or_default();
        if text.contains("VML107:WRITTEN") {
            break;
        }
        assert!(!text.contains("VML107:FAILED"), "ATA write failed\n{}", logs());
        assert!(
            child.try_wait().unwrap().is_none() && Instant::now() < deadline,
            "the ROM did not finish its writes\n{}",
            logs()
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    // The managed child leads its own process group: vmlab-hv, after
    // systemd-run's exec. SIGTERM goes to that group only.
    let pid = child.id() as i32;
    // SAFETY: kill(2) on the child's own process group.
    assert_eq!(unsafe { libc::kill(-pid, libc::SIGTERM) }, 0);
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "no exit after SIGTERM\n{}", logs());
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(
        status.code(),
        Some(128 + libc::SIGTERM),
        "SIGTERM must stop the machine cleanly\n{}",
        logs()
    );

    let check = qemu_img(&["check".as_ref(), disk.as_os_str()]);
    assert!(check.contains("No errors were found"), "{check}");
    let raw = path.join("disk.raw");
    qemu_img(&[
        "convert".as_ref(),
        "-f".as_ref(),
        "qcow2".as_ref(),
        "-O".as_ref(),
        "raw".as_ref(),
        disk.as_os_str(),
        raw.as_os_str(),
    ]);
    let file = std::fs::File::open(&raw).unwrap();
    for (lba, word) in WRITES {
        let mut sector = [0u8; 512];
        std::os::unix::fs::FileExt::read_exact_at(&file, &mut sector, u64::from(lba) * 512)
            .unwrap();
        let expected: Vec<u8> = std::iter::repeat_n(word.to_le_bytes(), 256)
            .flatten()
            .collect();
        assert!(
            sector[..] == expected[..],
            "sector {lba} is not the guest's write after SIGTERM (first bytes {:02x?})\n{check}",
            &sector[..8]
        );
    }
}
