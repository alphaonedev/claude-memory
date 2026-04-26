// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! # Public API
//!
//! `CliOutput` is the parameterized output abstraction every `cmd_*`
//! handler writes to. It owns mutable references to `dyn Write` for
//! both stdout and stderr so production code can pass `io::stdout()` /
//! `io::stderr()` locks while unit tests pass `Vec<u8>` capture buffers.
//!
//! The struct is the **stable contract** between W5a (this module) and
//! the downstream cmd_* migrations in W5b/c/d. Do not change the field
//! visibility or method signatures without coordinating across closers.
//!
//! ## Stable surface
//!
//! ```ignore
//! pub struct CliOutput<'a> {
//!     pub stdout: &'a mut dyn Write,
//!     pub stderr: &'a mut dyn Write,
//! }
//!
//! impl<'a> CliOutput<'a> {
//!     pub fn from_std(stdout: &'a mut dyn Write, stderr: &'a mut dyn Write) -> Self;
//! }
//! ```
//!
//! ## Usage in handlers
//!
//! Every `cmd_*` replaces `println!(...)` with `writeln!(out.stdout, ...)?`
//! and `eprintln!(...)` with `writeln!(out.stderr, ...)?`. The `?`
//! propagates I/O errors instead of panicking on broken-pipe (closing
//! a long-running pager mid-output, etc.).

use std::io::Write;

/// Output abstraction passed to every CLI command. Carries mutable
/// references to stdout and stderr writers so handlers can be unit-tested
/// by capturing into `Vec<u8>` buffers.
pub struct CliOutput<'a> {
    pub stdout: &'a mut dyn Write,
    pub stderr: &'a mut dyn Write,
}

impl<'a> CliOutput<'a> {
    /// Construct from explicit stdout/stderr writer references. Both must
    /// outlive the resulting `CliOutput` borrow.
    pub fn from_std(stdout: &'a mut dyn Write, stderr: &'a mut dyn Write) -> Self {
        Self { stdout, stderr }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_capture_roundtrip() {
        let mut stdout = Vec::<u8>::new();
        let mut stderr = Vec::<u8>::new();
        let out = CliOutput {
            stdout: &mut stdout,
            stderr: &mut stderr,
        };
        writeln!(out.stdout, "hello").unwrap();
        writeln!(out.stderr, "warn").unwrap();
        assert_eq!(String::from_utf8(stdout).unwrap(), "hello\n");
        assert_eq!(String::from_utf8(stderr).unwrap(), "warn\n");
    }

    #[test]
    fn test_from_std_constructor() {
        let mut stdout = Vec::<u8>::new();
        let mut stderr = Vec::<u8>::new();
        {
            let out = CliOutput::from_std(&mut stdout, &mut stderr);
            writeln!(out.stdout, "ok").unwrap();
            writeln!(out.stderr, "err").unwrap();
        }
        assert_eq!(String::from_utf8(stdout).unwrap(), "ok\n");
        assert_eq!(String::from_utf8(stderr).unwrap(), "err\n");
    }

    #[test]
    fn test_independent_streams() {
        let mut stdout = Vec::<u8>::new();
        let mut stderr = Vec::<u8>::new();
        {
            let out = CliOutput::from_std(&mut stdout, &mut stderr);
            writeln!(out.stdout, "one").unwrap();
            writeln!(out.stdout, "two").unwrap();
            writeln!(out.stderr, "warn-1").unwrap();
        }
        assert_eq!(String::from_utf8(stdout).unwrap(), "one\ntwo\n");
        assert_eq!(String::from_utf8(stderr).unwrap(), "warn-1\n");
    }
}
