// SPDX-FileCopyrightText: Copyright 2026 silverbaum
// SPDX-License-Identifier: MIT

use nix::sys::inotify::{AddWatchFlags, InitFlags, Inotify};
use std::{
    ffi::OsString,
    fs::{self, OpenOptions, Permissions},
    io,
    os::unix::{
        ffi::OsStringExt,
        fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt, symlink},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Command, Output},
};

struct TestDir(PathBuf);

impl TestDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("rsmkswap-test-{}", uuid::Uuid::new_v4()));
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .expect("create test directory");
        Self(path)
    }

    fn join(&self, path: impl AsRef<Path>) -> PathBuf {
        self.0.join(path)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn cli() -> Command {
    Command::new(env!("CARGO_BIN_EXE_rsmkswap"))
}

fn run(command: &mut Command) -> Output {
    command.output().expect("run rsmkswap")
}

fn assert_exit(output: &Output, code: i32) {
    assert_eq!(
        output.status.code(),
        Some(code),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_error(output: &Output, message: &str) {
    assert_exit(output, 1);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(message), "{stderr}");
    assert!(output.stdout.is_empty());
}

#[test]
fn creates_a_full_length_swap_file_at_a_non_utf8_path() {
    let dir = TestDir::new();
    let path = dir.join(OsString::from_vec(b"swap-\xff".to_vec()));
    let pagesize = usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) })
        .expect("get the Linux page size");
    let size = 16 * pagesize - 1;
    let output = run(cli().args(["-F", "--size", &size.to_string()]).arg(&path));
    assert_exit(&output, 0);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(&format!("({} bytes)", 14 * pagesize)),
        "{stdout}"
    );

    let data = fs::read(&path).expect("read created swap file");
    assert_eq!(data.len(), size);
    assert_eq!(&data[1024..1028], &1u32.to_ne_bytes());
    assert_eq!(&data[1028..1032], &14u32.to_ne_bytes());
    assert_eq!(&data[pagesize - 10..pagesize], b"SWAPSPACE2");
    let metadata = fs::metadata(path).expect("stat created file");
    assert!(
        metadata.blocks() * 512 >= size as u64,
        "the entire swap file must be allocated"
    );
    assert_eq!(
        metadata.permissions().mode() & 0o177,
        0,
        "the new swap file must grant at most owner read/write permissions"
    );
}

#[test]
fn writes_big_endian_header_at_offset_without_touching_boot_bytes() {
    let dir = TestDir::new();
    let path = dir.join("swap");
    let mut original = vec![0xa5; 45056];
    fs::write(&path, &original).expect("seed existing swap file");
    let watch =
        Inotify::init(InitFlags::IN_NONBLOCK | InitFlags::IN_CLOEXEC).expect("init inotify");
    watch
        .add_watch(&path, AddWatchFlags::IN_ACCESS)
        .expect("watch for reads");
    let output = run(cli()
        .args([
            "--offset",
            "4096",
            "--pagesize",
            "4096",
            "--check",
            "--endianness",
            "big",
            "--label",
            "SWAPTEST",
            "--uuid",
            "123e4567-e89b-12d3-a456-426614174000",
        ])
        .arg(&path));
    assert_exit(&output, 0);

    assert!(
        watch
            .read_events()
            .expect("--check must read the target")
            .iter()
            .any(|event| event.mask.contains(AddWatchFlags::IN_ACCESS))
    );
    original[5120..8192].fill(0);
    original[5120..5132].copy_from_slice(&[0, 0, 0, 1, 0, 0, 0, 9, 0, 0, 0, 0]);
    original[5132..5148].copy_from_slice(&[
        0x12, 0x3e, 0x45, 0x67, 0xe8, 0x9b, 0x12, 0xd3, 0xa4, 0x56, 0x42, 0x66, 0x14, 0x17, 0x40,
        0x00,
    ]);
    original[5148..5156].copy_from_slice(b"SWAPTEST");
    original[8182..8192].copy_from_slice(b"SWAPSPACE2");
    let formatted = fs::read(path).expect("read formatted swap file");
    assert_eq!(formatted.len(), original.len());
    let difference = formatted
        .iter()
        .zip(&original)
        .position(|(actual, expected)| actual != expected);
    assert_eq!(difference, None, "first unexpected byte offset");
}

#[test]
fn create_refuses_existing_files_and_symlinks_without_modifying_them() {
    let dir = TestDir::new();
    let path = dir.join("existing");
    let link = dir.join("link");
    let original = vec![0xa5; 81920];
    fs::write(&path, &original).expect("seed existing file");
    fs::set_permissions(&path, Permissions::from_mode(0o644)).expect("set fixture mode");
    symlink(&path, &link).expect("create fixture symlink");

    for target in [&path, &link] {
        let output = run(cli().args(["-F", "-s", "81920", "-P", "4096"]).arg(target));
        assert_error(&output, "File exists");
        assert_eq!(fs::read(&path).expect("read original file"), original);
        assert_eq!(
            fs::metadata(&path)
                .expect("stat original file")
                .permissions()
                .mode()
                & 0o777,
            0o644
        );
    }
    assert_eq!(fs::read_link(link).expect("read unmodified symlink"), path);
}

#[test]
fn invalid_geometry_does_not_create_a_file() {
    let dir = TestDir::new();
    let path = dir.join("swap");
    for (size, offset) in [("1024", "0"), ("81920", "81920")] {
        let output = run(cli()
            .args(["-F", "-s", size, "-P", "4096", "-o", offset])
            .arg(&path));
        assert_error(&output, "Swap area needs to be at least 40 KiB");
        assert!(!path.exists());
    }
}

#[test]
fn failed_allocation_never_reports_success_or_writes_a_header() {
    let dir = TestDir::new();
    let path = dir.join("swap");
    let mut command = cli();
    command.args(["-F", "-s", "81920", "-P", "4096"]).arg(&path);
    unsafe {
        command.pre_exec(|| {
            let limit = libc::rlimit {
                rlim_cur: 4096,
                rlim_max: 4096,
            };
            if libc::signal(libc::SIGXFSZ, libc::SIG_IGN) == libc::SIG_ERR
                || libc::setrlimit(libc::RLIMIT_FSIZE, &limit) != 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let output = run(&mut command);
    assert_error(&output, "allocate");
    assert!(fs::read(path).expect("read failed allocation").is_empty());
}

#[test]
fn rejects_unsupported_targets() {
    let dir = TestDir::new();
    let path = dir.join("fifo");
    nix::unistd::mkfifo(&path, nix::sys::stat::Mode::S_IRUSR).expect("create test FIFO");
    let output = run(cli().arg(path));
    assert_error(&output, "is not a regular file or block device");
}

struct LoopDevice(PathBuf);

impl Drop for LoopDevice {
    fn drop(&mut self) {
        let _ = Command::new("losetup")
            .arg("--detach")
            .arg(&self.0)
            .status();
    }
}

#[test]
#[ignore = "requires root and Linux loop devices"]
fn formats_only_an_unclaimed_loop_device() {
    let dir = TestDir::new();
    let backing = dir.join("disk");
    let original = vec![0xa5; 65536];
    fs::write(&backing, &original).expect("create disposable disk image");
    let attached = Command::new("losetup")
        .args(["--find", "--show"])
        .arg(&backing)
        .output()
        .expect("attach disk image");
    assert_exit(&attached, 0);
    let device = LoopDevice(
        std::str::from_utf8(&attached.stdout)
            .expect("loop device name")
            .trim()
            .into(),
    );
    let alias = dir.join("swap-device");
    symlink(&device.0, &alias).expect("create device alias");

    let claim = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_EXCL)
        .open(&device.0)
        .expect("claim disposable loop device");
    let format = || run(cli().args(["-P", "4096"]).arg(&alias));
    assert_error(&format(), "Device or resource busy");
    assert!(fs::read(&backing).expect("read claimed disk image") == original);
    drop(claim);

    let formatted = format();
    assert_exit(&formatted, 0);
    assert!(String::from_utf8_lossy(&formatted.stdout).contains("(61440 bytes)"));
    let probe = Command::new("blkid")
        .args(["-p", "-s", "TYPE", "-o", "value"])
        .arg(&alias)
        .output()
        .expect("probe formatted device");
    assert_exit(&probe, 0);
    assert_eq!(probe.stdout, b"swap\n");
}

#[test]
fn warns_about_insecure_permissions_on_existing_files() {
    let dir = TestDir::new();
    let path = dir.join("swap");
    fs::write(&path, vec![0; 40960]).expect("seed existing file");
    for mode in [0o600, 0o644, 0o4600, 0o2600, 0o1600] {
        fs::set_permissions(&path, Permissions::from_mode(mode)).expect("set fixture mode");
        let output = run(cli().args(["-P", "4096"]).arg(&path));
        assert_exit(&output, 0);
        let stderr = String::from_utf8_lossy(&output.stderr);
        if mode == 0o600 {
            assert!(!stderr.contains("insecure file permissions"), "{stderr}");
        } else {
            assert!(
                stderr.contains(&format!("insecure file permissions {mode:04o}")),
                "{stderr}"
            );
        }
    }
}

#[test]
fn nonexistent_target_error_names_the_operation_and_path() {
    let dir = TestDir::new();
    let path = dir.join("missing");
    let output = run(cli().arg(&path));
    assert_error(
        &output,
        &format!("stat {}: No such file or directory", path.display()),
    );
}

#[test]
fn invalid_label_or_uuid_does_not_create_a_file() {
    let dir = TestDir::new();
    let path = dir.join("swap");
    for (flag, value, error) in [
        ("--label", "ééééééééé", "maximum size is 16 bytes"),
        ("--uuid", "invalid-uuid", "Invalid UUID 'invalid-uuid'"),
    ] {
        let output = run(cli().args(["-F", "-s", "81920", flag, value]).arg(&path));
        assert_error(&output, error);
        assert!(!path.exists());
    }
}

#[test]
fn clap_enforces_creation_arguments_and_reports_usage_on_stderr() {
    let dir = TestDir::new();
    let path = dir.join("swap");
    for (args, missing) in [
        (&["--file"][..], "--size <SIZE>"),
        (&["--size", "81920"][..], "--file"),
    ] {
        let output = run(cli().args(args).arg(&path));
        assert_exit(&output, 2);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("Usage:") && stderr.contains(missing),
            "{stderr}"
        );
        assert!(output.stdout.is_empty());
        assert!(!path.exists());
    }
}
