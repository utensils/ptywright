use std::borrow::Cow;
use std::io::{self, IsTerminal, Read, Write};
use std::process::ExitCode;
use std::thread;

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use ptywright::{Target, TerminalSize};

pub fn run_command(command: Vec<String>, size: TerminalSize) -> ptywright::Result<ExitCode> {
    let (program, args) = command
        .split_first()
        .ok_or_else(|| ptywright::Error::Rpc("run requires a command after `--`".to_string()))?;
    let target = Target::new(program.clone())
        .args(args.iter().cloned())
        .size(size);
    let interactive_terminal = io::stdin().is_terminal() && io::stdout().is_terminal();
    let _raw_mode = RawModeGuard::enable_if(interactive_terminal)?;

    let pty_system = native_pty_system();
    let pair = pty_system.openpty(PtySize {
        rows: target.size.rows,
        cols: target.size.cols,
        pixel_width: target.size.pixel_width,
        pixel_height: target.size.pixel_height,
    })?;

    let mut builder = CommandBuilder::new(&target.program);
    builder.args(target.args.iter().map(String::as_str));
    let mut child = pair.slave.spawn_command(builder)?;
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader()?;
    let mut writer = pair.master.take_writer()?;

    let output_thread = thread::spawn(move || -> io::Result<()> {
        let mut stdout = io::stdout().lock();
        let mut buf = [0_u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    stdout.write_all(&buf[..n])?;
                    stdout.flush()?;
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        Ok(())
    });

    let _input_thread = thread::spawn(move || -> io::Result<()> {
        let mut stdin = io::stdin().lock();
        let mut filter = TerminalInputFilter::new();
        let mut buf = [0_u8; 8192];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) => {
                    let pending = filter.finish();
                    if !pending.is_empty() {
                        writer.write_all(&pending)?;
                        writer.flush()?;
                    }
                    break;
                }
                Ok(n) => {
                    if interactive_terminal {
                        let input = filter.filter(&buf[..n]);
                        if !input.is_empty() {
                            writer.write_all(&input)?;
                            writer.flush()?;
                        }
                    } else {
                        writer.write_all(&buf[..n])?;
                        writer.flush()?;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        Ok(())
    });

    let status = child.wait()?;
    drop(pair.master);
    if let Ok(Err(error)) = output_thread.join() {
        return Err(error.into());
    }

    if status.success() {
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(ExitCode::from(status.exit_code().min(u8::MAX as u32) as u8))
    }
}

struct RawModeGuard {
    enabled: bool,
}

impl RawModeGuard {
    fn enable_if(enabled: bool) -> io::Result<Self> {
        if enabled {
            crossterm::terminal::enable_raw_mode()?;
        }
        Ok(Self { enabled })
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        if self.enabled {
            let _ = crossterm::terminal::disable_raw_mode();
        }
    }
}

struct TerminalInputFilter {
    pending: Vec<u8>,
}

impl TerminalInputFilter {
    fn new() -> Self {
        Self {
            pending: Vec::new(),
        }
    }

    fn filter<'a>(&mut self, input: &'a [u8]) -> Cow<'a, [u8]> {
        if self.pending.is_empty() {
            let mut index = 0;
            while index < input.len() {
                match terminal_generated_sequence_status(&input[index..]) {
                    SequenceStatus::Complete(_) | SequenceStatus::Incomplete => {
                        let filtered = filter_terminal_generated_input_with_pending(input);
                        self.pending = filtered.pending;
                        return Cow::Owned(filtered.output);
                    }
                    SequenceStatus::NotGenerated => index += 1,
                }
            }
            Cow::Borrowed(input)
        } else {
            self.pending.extend_from_slice(input);
            let data = std::mem::take(&mut self.pending);
            let filtered = filter_terminal_generated_input_with_pending(&data);
            self.pending = filtered.pending;
            Cow::Owned(filtered.output)
        }
    }

    fn finish(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending)
    }
}

struct FilteredInput {
    output: Vec<u8>,
    pending: Vec<u8>,
}

#[cfg(test)]
fn filter_terminal_generated_input(input: &[u8]) -> Cow<'_, [u8]> {
    let mut index = 0;
    while index < input.len() {
        match terminal_generated_sequence_status(&input[index..]) {
            SequenceStatus::Complete(_) | SequenceStatus::Incomplete => {
                let filtered = filter_terminal_generated_input_with_pending(input);
                let mut output = filtered.output;
                output.extend_from_slice(&filtered.pending);
                return Cow::Owned(output);
            }
            SequenceStatus::NotGenerated => index += 1,
        }
    }
    Cow::Borrowed(input)
}

fn filter_terminal_generated_input_with_pending(input: &[u8]) -> FilteredInput {
    let mut index = 0;
    let mut output: Option<Vec<u8>> = None;

    while index < input.len() {
        match terminal_generated_sequence_status(&input[index..]) {
            SequenceStatus::Complete(len) => {
                output.get_or_insert_with(|| input[..index].to_vec());
                index += len;
            }
            SequenceStatus::Incomplete => {
                return FilteredInput {
                    output: output.unwrap_or_else(|| input[..index].to_vec()),
                    pending: input[index..].to_vec(),
                };
            }
            SequenceStatus::NotGenerated => {
                if let Some(output) = &mut output {
                    output.push(input[index]);
                }
                index += 1;
            }
        }
    }

    FilteredInput {
        output: output.unwrap_or_else(|| input.to_vec()),
        pending: Vec::new(),
    }
}

enum SequenceStatus {
    Complete(usize),
    Incomplete,
    NotGenerated,
}

fn terminal_generated_sequence_status(input: &[u8]) -> SequenceStatus {
    if input_starts_incomplete_generated_sequence(input) {
        return SequenceStatus::Incomplete;
    }
    match terminal_generated_sequence_len(input) {
        Some(len) => SequenceStatus::Complete(len),
        None => SequenceStatus::NotGenerated,
    }
}

fn input_starts_incomplete_generated_sequence(input: &[u8]) -> bool {
    matches!(
        input,
        [0x1b, b'['] | [0x1b, b'[', b'?'] | [0x1b, b'P'] | [0x9b] | [0x9b, b'?'] | [0x90]
    ) || matches!(input, [0x1b, b'[', b'?', rest @ ..] if csi_final_len(rest).is_none())
        || matches!(input, [0x1b, b'P', rest @ ..] if dcs_final_len(rest).is_none())
        || matches!(input, [0x9b, b'?', rest @ ..] if csi_final_len(rest).is_none())
        || matches!(input, [0x90, rest @ ..] if dcs_final_len(rest).is_none())
}

fn terminal_generated_sequence_len(input: &[u8]) -> Option<usize> {
    match input {
        [0x1b, b'[', b'I', ..] | [0x1b, b'[', b'O', ..] => Some(3),
        [0x1b, b'[', b'?', rest @ ..] => csi_final_len(rest).and_then(|len| {
            if rest.get(len - 1) == Some(&b'c') {
                Some(3 + len)
            } else {
                None
            }
        }),
        [0x1b, b'P', rest @ ..] => dcs_final_len(rest).map(|len| 2 + len),
        [0x9b, b'I', ..] | [0x9b, b'O', ..] => Some(2),
        [0x9b, b'?', rest @ ..] => csi_final_len(rest).and_then(|len| {
            if rest.get(len - 1) == Some(&b'c') {
                Some(2 + len)
            } else {
                None
            }
        }),
        [0x90, rest @ ..] => dcs_final_len(rest).map(|len| 1 + len),
        _ => None,
    }
}

fn csi_final_len(input: &[u8]) -> Option<usize> {
    input
        .iter()
        .position(|byte| (0x40..=0x7e).contains(byte))
        .map(|index| index + 1)
}

fn dcs_final_len(input: &[u8]) -> Option<usize> {
    let mut index = 0;
    while index < input.len() {
        match input[index] {
            0x07 | 0x9c => return Some(index + 1),
            0x1b if input.get(index + 1) == Some(&b'\\') => return Some(index + 2),
            _ => index += 1,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_terminal_focus_events() {
        assert_eq!(
            filter_terminal_generated_input(b"a\x1b[Ib\x1b[Oc").as_ref(),
            b"abc"
        );
    }

    #[test]
    fn filters_terminal_capability_responses() {
        let input = b"hello\x1bP>|ghostty 1.3.1\x1b\\\x1b[?62;22;52cworld";

        assert_eq!(
            filter_terminal_generated_input(input).as_ref(),
            b"helloworld"
        );
    }

    #[test]
    fn preserves_user_navigation_sequences() {
        let input = b"\x1b[A\x1b[B\x1b[200~paste\x1b[201~";

        assert_eq!(filter_terminal_generated_input(input).as_ref(), input);
    }

    #[test]
    fn filters_8_bit_terminal_generated_sequences() {
        let input = b"a\x9bIb\x9bOc\x9b?62;22;52cd\x90>|term\x9ce";

        assert_eq!(filter_terminal_generated_input(input).as_ref(), b"abcde");
    }

    #[test]
    fn preserves_incomplete_or_user_csi_dcs_sequences() {
        let input = b"\x1b[?25h\x1bPunterminated";

        assert_eq!(filter_terminal_generated_input(input).as_ref(), input);
    }

    #[test]
    fn stateful_filter_handles_split_generated_sequences() {
        let mut filter = TerminalInputFilter::new();

        assert_eq!(filter.filter(b"hello\x1bP>|ghost").as_ref(), b"hello");
        assert_eq!(filter.filter(b"ty\x1b\\world").as_ref(), b"world");
        assert!(filter.finish().is_empty());
    }

    #[test]
    fn stateful_filter_flushes_incomplete_pending_input() {
        let mut filter = TerminalInputFilter::new();

        assert_eq!(filter.filter(b"\x1bPunterminated").as_ref(), b"");
        assert_eq!(filter.finish(), b"\x1bPunterminated");
    }

    #[test]
    fn raw_mode_guard_noops_when_disabled() {
        let guard = RawModeGuard::enable_if(false).expect("disabled raw mode guard");

        assert!(!guard.enabled);
    }

    #[test]
    fn run_command_rejects_empty_command() {
        let error = run_command(Vec::new(), TerminalSize::new(24, 80)).expect_err("empty command");

        assert!(matches!(error, ptywright::Error::Rpc(_)));
        assert!(error.to_string().contains("requires a command"));
    }
}
