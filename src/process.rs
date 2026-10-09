//! Hidden PowerShell worker with a JSON input/output boundary; paths never become code.
use anyhow::{bail, Context, Result};
use base64::Engine;
use serde_json::Value;
use std::process::Command;

pub fn powershell(script: &str, payload: &Value) -> Result<Value> {
    #[cfg(not(windows))]
    {
        let _ = (script, payload);
        bail!("该操作仅支持 Windows");
    }
    #[cfg(windows)]
    {
        let text = wrap_script(script);
        let encoded = encode_script(&text);
        let executable = std::env::var_os("SystemRoot")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| "C:\\Windows".into())
            .join("System32\\WindowsPowerShell\\v1.0\\powershell.exe");
        let mut command = Command::new(executable);
        command.args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-EncodedCommand",
            &encoded,
        ]);
        command.env("VHDXDOCK_REQUEST", serde_json::to_string(payload)?);
        hide_window(&mut command);
        let output = command.output().context("无法启动 Windows PowerShell")?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !output.status.success() {
            bail!(
                "Windows 操作失败：{}{}",
                stderr.trim(),
                if stderr.trim().is_empty() {
                    stdout.trim()
                } else {
                    ""
                }
            );
        }
        let text = stdout.trim().trim_start_matches('\u{feff}');
        if text.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(text).with_context(|| format!("Windows 返回了无效的 JSON：{text}"))
    }
}

pub fn hide_window(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    #[cfg(not(windows))]
    {
        let _ = command;
    }
}

pub fn wrap_script(script: &str) -> String {
    format!("$ErrorActionPreference = 'Stop'\n$ProgressPreference = 'SilentlyContinue'\n[Console]::OutputEncoding = New-Object System.Text.UTF8Encoding($false)\ntry {{\n$req = ConvertFrom-Json $env:VHDXDOCK_REQUEST\n{script}\n}} catch {{\n[Console]::Error.WriteLine($_.Exception.Message)\nexit 1\n}}")
}

fn encode_script(text: &str) -> String {
    let bytes: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn encoded_script_is_utf16() {
        let input = "中文 $req.path";
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encode_script(input))
            .unwrap();
        let words: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|v| u16::from_le_bytes([v[0], v[1]]))
            .collect();
        assert_eq!(String::from_utf16(&words).unwrap(), input);
    }
    #[test]
    fn payload_is_not_interpolated() {
        let script = wrap_script("$req.path | ConvertTo-Json");
        assert!(script.contains("ConvertFrom-Json $env:VHDXDOCK_REQUEST"));
    }
}
