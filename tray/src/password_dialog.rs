use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::process::{Command, Stdio};
use zeroize::Zeroizing;

const MAX_PASSWORD_BYTES: usize = 16 * 1024;
#[cfg(any(target_os = "macos", test))]
const APPLE_OK_PREFIX: &str = "feanorfs-dialog-ok:";
#[cfg(any(target_os = "macos", test))]
const APPLE_CANCEL: &str = "feanorfs-dialog-cancel";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OutputProtocol {
    #[cfg(any(target_os = "linux", target_os = "windows", test))]
    Plain,
    #[cfg(any(target_os = "macos", test))]
    AppleScript,
}

#[derive(Debug)]
struct DialogCommand {
    program: OsString,
    args: Vec<OsString>,
    env: Vec<(OsString, OsString)>,
    stdin: &'static [u8],
    protocol: OutputProtocol,
    cancel_exit_code: Option<i32>,
}

#[derive(Debug)]
struct DialogOutput {
    success: bool,
    exit_code: Option<i32>,
    stdout: Vec<u8>,
}

pub(crate) fn prompt(title: &str, message: &str) -> Result<Option<Zeroizing<String>>, String> {
    prompt_with(title, message, run_dialog)
}

fn prompt_with<F>(
    title: &str,
    message: &str,
    mut runner: F,
) -> Result<Option<Zeroizing<String>>, String>
where
    F: FnMut(&DialogCommand) -> io::Result<DialogOutput>,
{
    let commands = platform_commands(title, message);
    let command_count = commands.len();

    for (index, command) in commands.into_iter().enumerate() {
        match runner(&command) {
            Ok(output) => return parse_output(&command, output),
            Err(error) if error.kind() == io::ErrorKind::NotFound && index + 1 < command_count => {
                continue;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(missing_dialog_message());
            }
            Err(error) => {
                return Err(format!(
                    "FeanorFS could not start the operating-system password dialog: {error}"
                ));
            }
        }
    }

    Err(missing_dialog_message())
}

fn run_dialog(command: &DialogCommand) -> io::Result<DialogOutput> {
    let mut process = Command::new(&command.program);
    process
        .args(&command.args)
        .envs(command.env.iter().cloned())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        process.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = process.spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        if let Err(error) = stdin.write_all(command.stdin) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
    }
    let mut stdout = Vec::with_capacity(MAX_PASSWORD_BYTES + 1);
    if let Some(child_stdout) = child.stdout.take() {
        if let Err(error) = child_stdout
            .take((MAX_PASSWORD_BYTES + 1) as u64)
            .read_to_end(&mut stdout)
        {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
    }
    if stdout.len() > MAX_PASSWORD_BYTES {
        let _ = child.kill();
    }
    let status = child.wait()?;

    Ok(DialogOutput {
        success: status.success(),
        exit_code: status.code(),
        stdout,
    })
}

fn parse_output(
    command: &DialogCommand,
    output: DialogOutput,
) -> Result<Option<Zeroizing<String>>, String> {
    let stdout = Zeroizing::new(output.stdout);
    if stdout.len() > MAX_PASSWORD_BYTES {
        return Err("The password dialog returned more data than FeanorFS accepts.".into());
    }

    if command.cancel_exit_code.is_some()
        && command.cancel_exit_code == output.exit_code
        && !output.success
    {
        return Ok(None);
    }
    if !output.success {
        return Err(match output.exit_code {
            Some(code) => format!("The operating-system password dialog failed (exit {code})."),
            None => "The operating-system password dialog was interrupted.".into(),
        });
    }

    let value = std::str::from_utf8(&stdout)
        .map_err(|_| "The operating-system password dialog returned invalid text.".to_string())?;
    let value = Zeroizing::new(strip_one_line_ending(value.to_owned()));

    match command.protocol {
        #[cfg(any(target_os = "linux", target_os = "windows", test))]
        OutputProtocol::Plain => Ok(Some(value)),
        #[cfg(any(target_os = "macos", test))]
        OutputProtocol::AppleScript if value.as_str() == APPLE_CANCEL => Ok(None),
        #[cfg(any(target_os = "macos", test))]
        OutputProtocol::AppleScript => value
            .strip_prefix(APPLE_OK_PREFIX)
            .map(|password| Some(Zeroizing::new(password.to_string())))
            .ok_or_else(|| "The macOS password dialog returned an invalid response.".into()),
    }
}

fn strip_one_line_ending(mut value: String) -> String {
    if value.ends_with('\n') {
        value.pop();
        if value.ends_with('\r') {
            value.pop();
        }
    }
    value
}

#[cfg(target_os = "macos")]
fn platform_commands(title: &str, message: &str) -> Vec<DialogCommand> {
    const SCRIPT: &[u8] = br#"on run argv
    set dialogTitle to item 1 of argv
    set dialogMessage to item 2 of argv
    try
        set response to display dialog dialogMessage with title dialogTitle default answer "" with hidden answer buttons {"Cancel", "Continue"} default button "Continue" cancel button "Cancel"
        return "feanorfs-dialog-ok:" & text returned of response
    on error errorMessage number errorNumber
        if errorNumber is -128 then
            return "feanorfs-dialog-cancel"
        end if
        error errorMessage number errorNumber
    end try
end run
"#;

    vec![DialogCommand {
        program: OsString::from("/usr/bin/osascript"),
        args: vec![OsString::from("-"), title.into(), message.into()],
        env: Vec::new(),
        stdin: SCRIPT,
        protocol: OutputProtocol::AppleScript,
        cancel_exit_code: None,
    }]
}

#[cfg(target_os = "windows")]
fn platform_commands(title: &str, message: &str) -> Vec<DialogCommand> {
    const SCRIPT: &[u8] = br#"Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
if ([string]::IsNullOrEmpty($env:FEANORFS_DIALOG_TITLE)) { exit 3 }
if ([string]::IsNullOrEmpty($env:FEANORFS_DIALOG_MESSAGE)) { exit 3 }

$form = New-Object System.Windows.Forms.Form
$form.Text = $env:FEANORFS_DIALOG_TITLE
$form.ClientSize = New-Object System.Drawing.Size(430, 145)
$form.FormBorderStyle = [System.Windows.Forms.FormBorderStyle]::FixedDialog
$form.MaximizeBox = $false
$form.MinimizeBox = $false
$form.ShowIcon = $false
$form.StartPosition = [System.Windows.Forms.FormStartPosition]::CenterScreen
$form.TopMost = $true

$label = New-Object System.Windows.Forms.Label
$label.Text = $env:FEANORFS_DIALOG_MESSAGE
$label.Location = New-Object System.Drawing.Point(18, 18)
$label.Size = New-Object System.Drawing.Size(394, 24)
$form.Controls.Add($label)

$passwordBox = New-Object System.Windows.Forms.TextBox
$passwordBox.Location = New-Object System.Drawing.Point(18, 48)
$passwordBox.Size = New-Object System.Drawing.Size(394, 23)
$passwordBox.UseSystemPasswordChar = $true
$form.Controls.Add($passwordBox)

$continue = New-Object System.Windows.Forms.Button
$continue.Text = "Continue"
$continue.Location = New-Object System.Drawing.Point(236, 94)
$continue.Size = New-Object System.Drawing.Size(84, 30)
$continue.DialogResult = [System.Windows.Forms.DialogResult]::OK
$form.Controls.Add($continue)
$form.AcceptButton = $continue

$cancel = New-Object System.Windows.Forms.Button
$cancel.Text = "Cancel"
$cancel.Location = New-Object System.Drawing.Point(328, 94)
$cancel.Size = New-Object System.Drawing.Size(84, 30)
$cancel.DialogResult = [System.Windows.Forms.DialogResult]::Cancel
$form.Controls.Add($cancel)
$form.CancelButton = $cancel

$form.Add_Shown({ $passwordBox.Select() })
$result = $form.ShowDialog()
if ($result -ne [System.Windows.Forms.DialogResult]::OK) { exit 2 }
[Console]::Out.Write($passwordBox.Text)
"#;

    let system_root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
    let powershell = std::path::PathBuf::from(system_root)
        .join("System32")
        .join("WindowsPowerShell")
        .join("v1.0")
        .join("powershell.exe");

    vec![DialogCommand {
        program: powershell.into_os_string(),
        args: vec![
            "-NoLogo".into(),
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-WindowStyle".into(),
            "Hidden".into(),
            "-Command".into(),
            "-".into(),
        ],
        env: vec![
            ("FEANORFS_DIALOG_TITLE".into(), title.into()),
            ("FEANORFS_DIALOG_MESSAGE".into(), message.into()),
        ],
        stdin: SCRIPT,
        protocol: OutputProtocol::Plain,
        cancel_exit_code: Some(2),
    }]
}

#[cfg(target_os = "linux")]
fn platform_commands(title: &str, message: &str) -> Vec<DialogCommand> {
    vec![
        DialogCommand {
            program: OsString::from("/usr/bin/zenity"),
            args: vec![
                "--password".into(),
                "--title".into(),
                title.into(),
                "--text".into(),
                message.into(),
                "--no-markup".into(),
            ],
            env: Vec::new(),
            stdin: &[],
            protocol: OutputProtocol::Plain,
            cancel_exit_code: Some(1),
        },
        DialogCommand {
            program: OsString::from("/usr/bin/kdialog"),
            args: vec![
                "--password".into(),
                message.into(),
                "--title".into(),
                title.into(),
            ],
            env: Vec::new(),
            stdin: &[],
            protocol: OutputProtocol::Plain,
            cancel_exit_code: Some(1),
        },
    ]
}

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
fn platform_commands(_title: &str, _message: &str) -> Vec<DialogCommand> {
    Vec::new()
}

#[cfg(target_os = "linux")]
fn missing_dialog_message() -> String {
    "FeanorFS needs zenity (preferred) or kdialog for masked password prompts. Install the native FeanorFS package to receive this dependency automatically.".into()
}

#[cfg(not(target_os = "linux"))]
fn missing_dialog_message() -> String {
    "FeanorFS could not find the operating-system password dialog.".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TITLE: &str = "FeanorFS recovery";
    const MESSAGE: &str = "Recovery kit passphrase";
    const SECRET: &str = "do-not-place-this-in-process-metadata";

    fn accepted_output(command: &DialogCommand) -> DialogOutput {
        let stdout = match command.protocol {
            OutputProtocol::Plain => format!("{SECRET}\n"),
            OutputProtocol::AppleScript => format!("{APPLE_OK_PREFIX}{SECRET}\n"),
        };
        DialogOutput {
            success: true,
            exit_code: Some(0),
            stdout: stdout.into_bytes(),
        }
    }

    #[test]
    fn secret_is_read_only_from_captured_output() {
        let result = prompt_with(TITLE, MESSAGE, |command| {
            assert!(!command.program.to_string_lossy().contains(SECRET));
            assert!(command
                .args
                .iter()
                .all(|argument| !argument.to_string_lossy().contains(SECRET)));
            assert!(command.env.iter().all(|(name, value)| {
                !name.to_string_lossy().contains(SECRET)
                    && !value.to_string_lossy().contains(SECRET)
            }));
            assert!(!String::from_utf8_lossy(command.stdin).contains(SECRET));
            Ok(accepted_output(command))
        })
        .unwrap()
        .unwrap();

        assert_eq!(result.as_str(), SECRET);
    }

    #[test]
    fn oversized_or_invalid_output_is_rejected() {
        let command = &platform_commands(TITLE, MESSAGE)[0];
        assert!(parse_output(
            command,
            DialogOutput {
                success: true,
                exit_code: Some(0),
                stdout: vec![b'x'; MAX_PASSWORD_BYTES + 1],
            }
        )
        .is_err());
        assert!(parse_output(
            command,
            DialogOutput {
                success: true,
                exit_code: Some(0),
                stdout: vec![0xff],
            }
        )
        .is_err());
    }

    #[test]
    fn both_output_protocols_decode_only_accepted_text() {
        let plain = DialogCommand {
            program: OsString::from("dialog"),
            args: Vec::new(),
            env: Vec::new(),
            stdin: &[],
            protocol: OutputProtocol::Plain,
            cancel_exit_code: Some(1),
        };
        let apple = DialogCommand {
            program: OsString::from("dialog"),
            args: Vec::new(),
            env: Vec::new(),
            stdin: &[],
            protocol: OutputProtocol::AppleScript,
            cancel_exit_code: None,
        };

        let decoded_plain = parse_output(
            &plain,
            DialogOutput {
                success: true,
                exit_code: Some(0),
                stdout: format!("{SECRET}\n").into_bytes(),
            },
        )
        .unwrap()
        .unwrap();
        let decoded_apple = parse_output(
            &apple,
            DialogOutput {
                success: true,
                exit_code: Some(0),
                stdout: format!("{APPLE_OK_PREFIX}{SECRET}\n").into_bytes(),
            },
        )
        .unwrap()
        .unwrap();

        assert_eq!(decoded_plain.as_str(), SECRET);
        assert_eq!(decoded_apple.as_str(), SECRET);
    }

    #[test]
    fn cancel_returns_no_password() {
        let command = &platform_commands(TITLE, MESSAGE)[0];
        let output = if command.protocol == OutputProtocol::AppleScript {
            DialogOutput {
                success: true,
                exit_code: Some(0),
                stdout: format!("{APPLE_CANCEL}\n").into_bytes(),
            }
        } else {
            DialogOutput {
                success: false,
                exit_code: command.cancel_exit_code,
                stdout: Vec::new(),
            }
        };

        assert!(parse_output(command, output).unwrap().is_none());
    }

    #[test]
    fn command_uses_only_public_copy_and_a_static_script() {
        for command in platform_commands(TITLE, MESSAGE) {
            let args = command
                .args
                .iter()
                .map(|argument| argument.to_string_lossy())
                .collect::<Vec<_>>();
            let env_values = command
                .env
                .iter()
                .map(|(_, value)| value.to_string_lossy())
                .collect::<Vec<_>>();
            assert!(args.iter().chain(&env_values).any(|value| value == TITLE));
            assert!(args.iter().chain(&env_values).any(|value| value == MESSAGE));
            assert!(!command.stdin.is_empty() || cfg!(target_os = "linux"));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_falls_back_to_kdialog_when_zenity_is_missing() {
        let mut launches = 0;
        let result = prompt_with(TITLE, MESSAGE, |command| {
            launches += 1;
            if launches == 1 {
                return Err(io::Error::from(io::ErrorKind::NotFound));
            }
            assert_eq!(command.program, OsString::from("/usr/bin/kdialog"));
            Ok(accepted_output(command))
        })
        .unwrap()
        .unwrap();

        assert_eq!(launches, 2);
        assert_eq!(result.as_str(), SECRET);
    }
}
