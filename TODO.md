# TODO

Findings from codebase audit (`src/main.rs`, `tests/cli.rs`, `Cargo.toml`, `.github/workflows/ci.yml`, `README.md`).

This checklist tracks follow-ups to the original audit. The working tree addresses original findings 2, 4, 5, 6, 8, 9, 10, and 11. The `clap` migration, `thiserror`, and `tempfile` are optional.

## Correctness & Edge Cases

- [ ] **Fix `--check` read handling, bad-page limit, and usable capacity reporting** (`src/main.rs#L153-L157`, `src/main.rs#L224-L237`, `src/main.rs#L429-L453`)
  - [x] `check_device` starts with `Vec::new()` and scans data pages `1..pages`. Keep page 0 out of this scan; it cannot be listed as a bad page.
  - [x] `encode_header` keeps `last_page = pages - 1` and rejects a table that would overlap the signature. Its unit test covers 637 entries fitting and 638 failing at 4 KiB.
  - [ ] Complete short reads and retry `EINTR`. The current single `read_at` per page treats a short read, EOF, and every error as a bad page.
  - [ ] On a block device, record only `EIO` as a bad page. For a regular file, abort on `EIO` too: Linux rejects swap files with bad-page entries. Keep `ENODATA` and `EILSEQ` fatal. On EOF or a fatal error, stop before writing the header and report the path and offset of the failed read.
  - [ ] Share `max_badpages(pagesize)` between scanner and encoder. Stop before pushing the first entry that won't fit, including when the limit is zero (`--force -P 1546`). Currently only the encoder checks, after the scan.
  - [ ] Use `checked_sub` to calculate usable pages and reject zero before writing the header. Report `(pages - 1 - badpages.len()) * pagesize`; the current byte count includes bad pages.

- [ ] **Fix misleading `--force` help text** (`src/main.rs#L401`, `src/main.rs#L525-L530`)
  - `--help` claims `--force` allows swap size area to be larger than the device, but `forceflag` is only used to allow non-power-of-two `--pagesize`. Update the help string to match actual behavior.

- [ ] **Handle `fallocate` `EINTR` and cleanup on failed `-F` creation** (`src/main.rs#L304-L330`, `tests/cli.rs#L171-L193`)
  - [x] Allocation failure returns an error before writing the header. The CLI regression test asserts that the new file remains empty.
  - [x] Retry `fallocate` on `nix::errno::Errno::EINTR`.
  - [ ] Decide whether post-`create_new` failures (`fstatfs`, `disable_copy_on_write`, `fallocate`, `check_device`, `encode_header`, `write_all_at`, `sync_all`) should unlink the newly created file. There is no unlink on failure; update the allocation test if that policy changes.

- [ ] **Fix `getpagesize()` stale `errno` when `sysconf` returns `0`** (`src/main.rs#L174-L184`)
  - `io::Error::last_os_error()` is only meaningful when `sysconf` returns `-1`; use `nix::unistd::sysconf(SysconfVar::PAGE_SIZE)` or handle `size <= 0` without reading stale `errno`.

- [x] **Include special mode bits in insecure-permissions check** (`open_existing_target` in `src/main.rs`). The CLI test covers `04600`, `02600`, and `01600`.

- [x] **Document Linux UAPI ioctl type quirks** (`src/main.rs`). The request definitions explain why the encoded type sizes differ from the argument types.

## CLI & Error Handling

- [ ] **Consider migrating `clap` builder + `ArgMatches` to `#[derive(Parser)]`** (`src/main.rs#L360-L416`, `src/main.rs#L457-L547`, `Cargo.toml#L8`)
  - The CLI still uses string-keyed lookups, `.expect("required by Clap")`, and the `"cargo"` feature. There is no unused `"file"` flag read: Clap enforces `--file`/`--size` together and `mkswap` branches on `filesize`.
  - Use `#[derive(ValueEnum)]` for `Endian` (shows possible values in `--help`) and `value_parser!(Uuid)` if standardizing CLI parse failures on exit code `2` (would require updating `tests/cli.rs#L286-L297`).

- [ ] **Tighten `MkswapError` variants and messages** (`src/main.rs#L45-L116`)
  - Add an offset-bearing error variant (or optional offset field) for `--check` read failures, wired into `Error::source`.
  - Split runtime target/geometry errors out of `UsageError { msg: String }`.
  - Clarify `MaxBadPagesExceeded` display text (`"Too many bad pages: {max_badpages}"` → e.g. `"Too many bad pages (maximum is {max_badpages})"`).
  - Avoid misleading truncation in `SwapAreaTooSmall` (`min_swapsize >> 10`) for non-power-of-two `--force -P` sizes.
  - Align stderr diagnostic prefixes (`rsmkswap: `) on the user-specified pagesize warning (`src/main.rs#L408`) and verbose bad-page messages (`src/main.rs#L232`).
  - Optionally adopt `thiserror` to reduce `Display`/`Error` boilerplate and keep `source()` in sync when adding variants.

## Tests

- [ ] **Add scripted-reader unit tests for `--check`** (`src/main.rs`)
  - The offset CLI test runs `--check` on a healthy file. The bad-page unit tests pass indexes directly to the encoder; neither exercises a failed read.
  - Use a private `ReadAt` trait and scripted reader to cover short reads, `EINTR` retries, block-device vs. regular-file `EIO`, `UnexpectedEof` and fatal errors (asserting path + failing offset and no header write), `max_badpages` overflow (asserting early stop), `max_badpages == 0`, `usable_pages == 0`, and usable capacity subtraction.

- [x] **Strengthen existing integration tests** (`tests/cli.rs`). The tests assert that `0600` produces no permission warning, the non-UTF-8 fixture reports `14 * pagesize` usable bytes, and `--check` reads the target.

- [ ] **Fill missing boundary & flag test coverage** (`src/main.rs`, `tests/cli.rs`)
  - The CLI suite exercises a 10-page success, `offset == size`, an 18-byte label rejection, explicit big endian, and default native byte order. The exact cases below are still missing. CI has tmpfs and loop-device jobs but no Btrfs fixture.
  - `--pagesize` and `--force` validation (below `MIN_PAGE_SIZE`, non-power-of-two with/without `--force`, system pagesize mismatch warning).
  - `swap_pages` boundary unit tests (`pages == 9` vs `10`, `offset > size`, `pages > u32::MAX`).
  - `--label` exact byte-length boundary (`16` bytes accepted vs. `17` bytes rejected).
  - `--endianness` invalid value, `"little"`, `"native"`, and case-insensitivity.
  - Btrfs `FS_NOCOW_FL` path (`src/main.rs#L314-L316`, `src/main.rs#L332-L343`), if a btrfs fixture or CI check is practical.

- [ ] **Optionally replace hand-rolled `TestDir` with `tempfile::TempDir`** (`tests/cli.rs#L17-L38`)
  - Low priority since `TestDir` already uses UUIDs, `0o700`, and `Drop`, but `tempfile` is more standard in Rust test suites.
