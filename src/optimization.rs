//! Exclude child volumes from Windows' scheduled optimizer by stable volume GUID.
//! Change the scheduled action itself: registry-only preferences do not reliably
//! update the task. Never invoke defrag or disable optimization on the host disk.
use crate::process;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::json;

const SCHEDULE: &str = r#"
if ([string]$req.physical -notmatch '^\\\\\.\\PhysicalDrive(\d+)$') { throw '无法识别虚拟磁盘设备' }
$disk = Get-Disk -Number ([int]$Matches[1]) -ErrorAction Stop
if ($disk.IsBoot -or $disk.IsSystem -or $disk.PartitionStyle -eq 'RAW') { throw '拒绝修改系统磁盘或未分区磁盘的优化设置' }
$ids = @()
$aliases = @()
foreach ($p in @(Get-Partition -DiskNumber $disk.Number -ErrorAction Stop)) {
    if ($p.IsBoot -or $p.IsSystem) { throw '分区安全检查失败' }
    if ([string]$p.Type -eq 'Reserved') { continue }
    $v = @($p | Get-Volume -ErrorAction Stop)
    if ($v.Count -ne 1) { throw '无法唯一确认虚拟磁盘的数据卷' }
    $id = [string]$v[0].UniqueId
    if ($id -notmatch '^\\\\\?\\Volume\{[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\}\\$') { throw '数据卷没有有效的卷 GUID，拒绝使用宿主盘路径代替' }
    $ids += $id
    $aliases += @($p.AccessPaths) + @($id)
    if ($p.DriveLetter -and [string]$p.DriveLetter -match '^[A-Za-z]$') { $aliases += ([string]$p.DriveLetter + ':\') }
}
$ids = @($ids | Sort-Object -Unique)
if ($ids.Count -eq 0) { throw '虚拟磁盘没有可保护的数据卷' }
$taskPath = '\Microsoft\Windows\Defrag\'
$taskName = 'ScheduledDefrag'
function Read-OptimizerTask {
    $task = Get-ScheduledTask -TaskPath $taskPath -TaskName $taskName -ErrorAction Stop
    if ([string]$task.State -eq 'Running') { throw 'Windows 计划优化正在运行，请等待完成后重试挂载' }
    if (@($task.Actions).Count -ne 1) { throw '计划优化任务包含自定义动作，无法安全设置卷排除' }
    $exe = [Environment]::ExpandEnvironmentVariables([string]$task.Actions[0].Execute).Trim('"')
    $expected = Join-Path $env:SystemRoot 'System32\defrag.exe'
    if (-not [string]::Equals($exe, $expected, [StringComparison]::OrdinalIgnoreCase)) { throw '计划优化任务不是系统 defrag.exe，拒绝覆盖自定义任务' }
    return $task
}
$task = Read-OptimizerTask
$action = $task.Actions[0]
if ($req.apply) {
    if ([string]$disk.UniqueId -ne [string]$req.diskId -or @(Compare-Object $ids @($req.volumes)).Count) { throw '虚拟磁盘或卷身份发生变化，拒绝修改优化计划' }
    if ([string]$action.Arguments -cne [string]$req.before -or [string]$action.Execute -cne [string]$req.execute -or [string]$action.WorkingDirectory -cne [string]$req.directory) { throw '计划优化任务已被其他程序修改，请重试挂载' }
    $action.Arguments = [string]$req.arguments
    Set-ScheduledTask -TaskPath $taskPath -TaskName $taskName -Action @($action) -ErrorAction Stop | Out-Null
    $verified = Read-OptimizerTask
    if ([string]$verified.Actions[0].Arguments -cne [string]$req.arguments) { throw '计划优化排除设置写入后校验失败' }
}
ConvertTo-Json -InputObject @{volumes=@($ids);aliases=@($aliases | Where-Object { $_ } | Sort-Object -Unique);diskId=[string]$disk.UniqueId;arguments=[string]$task.Actions[0].Arguments;execute=[string]$action.Execute;directory=[string]$action.WorkingDirectory} -Depth 4 -Compress
"#;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Schedule {
    volumes: Vec<String>,
    aliases: Vec<String>,
    disk_id: String,
    arguments: String,
    execute: String,
    directory: String,
}

pub(crate) fn protect(physical: &str) -> Result<()> {
    let schedule: Schedule = serde_json::from_value(process::powershell(
        SCHEDULE,
        &json!({"physical": physical}),
    )?)?;
    let arguments = exclude_volumes(&schedule.arguments, &schedule.volumes, &schedule.aliases)?;
    if arguments != schedule.arguments {
        process::powershell(
            SCHEDULE,
            &json!({"physical":physical,"apply":true,"diskId":schedule.disk_id,
                "volumes":schedule.volumes,"before":schedule.arguments,
                "execute":schedule.execute,"directory":schedule.directory,
                "arguments":arguments}),
        )?;
    }
    Ok(())
}

fn key(path: &str) -> String {
    path.trim_end_matches('\\').to_ascii_lowercase()
}

fn split_arguments(arguments: &str) -> Result<Vec<String>> {
    use windows::{
        core::PCWSTR,
        Win32::{
            Foundation::{LocalFree, HLOCAL},
            UI::Shell::CommandLineToArgvW,
        },
    };
    let command = format!("defrag.exe {arguments}");
    if command.contains('\0') {
        bail!("计划优化参数含有空字符")
    }
    let wide: Vec<u16> = command.encode_utf16().chain(Some(0)).collect();
    let mut count = 0;
    let argv = unsafe { CommandLineToArgvW(PCWSTR(wide.as_ptr()), &mut count) };
    if argv.is_null() {
        bail!("无法解析 Windows 计划优化参数")
    }
    let result = unsafe { std::slice::from_raw_parts(argv, count as usize) }
        .iter()
        .skip(1)
        .map(|word| unsafe { word.to_string() }.context("计划优化参数编码无效"))
        .collect();
    unsafe { LocalFree(Some(HLOCAL(argv.cast()))) };
    result
}

fn quote_argument(word: &str) -> String {
    if !word.is_empty() && !word.chars().any(|c| c.is_whitespace() || c == '"') {
        return word.into();
    }
    let mut result = String::from("\"");
    let mut slashes = 0;
    for c in word.chars() {
        if c == '\\' {
            slashes += 1;
        } else {
            result.extend(std::iter::repeat_n(
                '\\',
                slashes * if c == '"' { 2 } else { 1 },
            ));
            slashes = 0;
            if c == '"' {
                result.push('\\');
            }
            result.push(c);
        }
    }
    result.extend(std::iter::repeat_n('\\', slashes * 2));
    result.push('"');
    result
}

fn exclude_volumes(arguments: &str, volumes: &[String], aliases: &[String]) -> Result<String> {
    let mut words = split_arguments(arguments)?;
    let mut mode = None;
    let mut volume_indices = Vec::new();
    let mut numeric = false;
    for (i, word) in words.iter().enumerate() {
        if numeric && word.parse::<u32>().is_ok() {
            numeric = false;
            continue;
        }
        numeric = false;
        if word.starts_with(['-', '/']) {
            let option = word[1..].to_ascii_lowercase();
            match option.as_str() {
                "c" | "allvolumes" | "e" | "volumesexcept" => {
                    if mode
                        .replace((i, option == "c" || option == "allvolumes"))
                        .is_some()
                    {
                        bail!("计划优化包含冲突的卷选择参数")
                    }
                }
                "m" | "i" => numeric = true,
                "a" | "b" | "d" | "g" | "h" | "k" | "l" | "o" | "t" | "u" | "v" | "x" | "$" => {}
                _ => bail!("计划优化包含不支持的自定义参数：{word}"),
            }
        } else {
            // Volumes may be GUID roots, drive roots or folder mount points.
            let bytes = word.as_bytes();
            if !(word.to_ascii_lowercase().starts_with(r"\\?\volume{")
                || (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'))
            {
                bail!("无法确认计划优化中的卷路径：{word}")
            }
            volume_indices.push(i);
        }
    }
    match mode {
        Some((index, true)) => {
            if !volume_indices.is_empty() {
                bail!("全部卷模式同时包含卷路径，拒绝覆盖自定义优化计划")
            }
            words[index] = "-e".into();
        }
        Some((_, false)) if volume_indices.is_empty() => bail!("计划优化的排除列表为空"),
        Some(_) => {}
        None => {
            if volume_indices.is_empty() {
                bail!("计划优化没有明确的卷选择参数")
            }
            let remove: Vec<_> = volume_indices
                .iter()
                .copied()
                .filter(|&i| aliases.iter().any(|alias| key(alias) == key(&words[i])))
                .collect();
            if remove.is_empty() {
                return Ok(arguments.into());
            }
            if remove.len() == volume_indices.len() {
                bail!("计划优化仅选择了当前虚拟卷；请先在 Windows 优化驱动器中取消这些卷的计划优化")
            }
            words = words
                .into_iter()
                .enumerate()
                .filter_map(|(i, word)| (!remove.contains(&i)).then_some(word))
                .collect();
            return Ok(words
                .iter()
                .map(|word| quote_argument(word))
                .collect::<Vec<_>>()
                .join(" "));
        }
    }
    let mut changed = matches!(mode, Some((_, true)));
    for volume in volumes {
        if !words.iter().any(|word| key(word) == key(volume)) {
            words.push(volume.clone());
            changed = true;
        }
    }
    if !changed {
        return Ok(arguments.into());
    }
    Ok(words
        .iter()
        .map(|word| quote_argument(word))
        .collect::<Vec<_>>()
        .join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;
    const CHILD: &str = r"\\?\Volume{11111111-1111-1111-1111-111111111111}\";
    const OTHER: &str = r"\\?\Volume{22222222-2222-2222-2222-222222222222}\";

    fn plan(arguments: &str) -> Result<String> {
        exclude_volumes(
            arguments,
            &[CHILD.into()],
            &[
                CHILD.into(),
                r"Z:\".into(),
                r"E:\Mounts\Archive space\".into(),
            ],
        )
    }

    #[test]
    fn excludes_volume_guid_and_preserves_existing_exclusions_and_flags() {
        let args = plan("-c -h -o -$").unwrap();
        assert_eq!(args, format!("-e -h -o -$ {CHILD}"));
        assert_eq!(plan(&args).unwrap(), args);
        let args = plan(&format!("/e {OTHER} /o /m 2 /i 30 -$")).unwrap();
        assert_eq!(args, format!("/e {OTHER} /o /m 2 /i 30 -$ {CHILD}"));
        assert_eq!(plan(&args).unwrap(), args);
        assert_eq!(plan(&args.to_uppercase()).unwrap(), args.to_uppercase());
    }

    #[test]
    fn inclusion_policy_keeps_unrelated_volumes_and_never_broadens_selection() {
        assert_eq!(plan(r"C: Z: /o -$").unwrap(), "C: /o -$");
        assert_eq!(
            plan(r#"C: "E:\Mounts\Archive space\\" /o"#).unwrap(),
            "C: /o"
        );
        assert_eq!(plan(r"C: D: /o").unwrap(), r"C: D: /o");
        assert!(plan("Z: /o").is_err());
        assert!(plan("/c /e Z:").is_err());
        assert!(plan("/e").is_err());
        assert!(plan("/c Z:").is_err());
        assert!(plan("/o").is_err());
        assert!(plan("/c -FileList custom.txt").is_err());
    }

    #[test]
    fn windows_arguments_roundtrip_spaces_unicode_quotes_and_trailing_slashes() {
        let words = [
            r"E:\挂载 folder\",
            r"\\?\Volume{fixture}\",
            "",
            r#"quote"inside"#,
        ];
        let joined = words
            .iter()
            .map(|word| quote_argument(word))
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(split_arguments(&joined).unwrap(), words);
    }

    // All Windows operations are replaced inside the worker process. These
    // tests never mount a disk, change a real task, or run an optimizer.
    const MOCKS: &str = r#"
$script:fixtureTask = [pscustomobject]@{State='Ready';Actions=@([pscustomobject]@{Execute='%windir%\system32\defrag.exe';Arguments='-c -h -o -$';WorkingDirectory=''})}
if ($req.scenario -eq 'running') { $script:fixtureTask.State = 'Running' }
if ($req.scenario -eq 'custom') { $script:fixtureTask.Actions[0].Execute = 'custom.exe' }
function Get-Disk { [pscustomobject]@{Number=999;UniqueId='scratch-disk';IsBoot=($req.scenario -eq 'boot');IsSystem=$false;PartitionStyle='GPT'} }
function Get-Partition {
    param($DiskNumber)
    if ($DiskNumber -ne 999) { throw 'must inspect the exact virtual disk' }
    [pscustomobject]@{IsBoot=$false;IsSystem=$false;Type='Reserved'}
    [pscustomobject]@{IsBoot=$false;IsSystem=$false;Type='Basic';DriveLetter='Z';AccessPaths=@('Z:\', 'E:\Mounts\Archive space\');Id='\\?\Volume{11111111-1111-1111-1111-111111111111}\'}
    [pscustomobject]@{IsBoot=$false;IsSystem=$false;Type='Basic';DriveLetter=$null;AccessPaths=@();Id='\\?\Volume{22222222-2222-2222-2222-222222222222}\'}
}
function Get-Volume {
    param([Parameter(ValueFromPipeline=$true)]$Partition)
    process {
        if ($Partition.Type -eq 'Reserved') { throw 'reserved partition must not be queried as a data volume' }
        $id = $Partition.Id
        if ($req.scenario -eq 'host-path') { $id = 'E:\' }
        [pscustomobject]@{UniqueId=$id}
    }
}
function Get-ScheduledTask {
    param($TaskPath,$TaskName)
    if ($TaskPath -ne '\Microsoft\Windows\Defrag\' -or $TaskName -ne 'ScheduledDefrag') { throw 'wrong task' }
    $script:fixtureTask
}
function Set-ScheduledTask {
    param($TaskPath,$TaskName,$Action)
    if ($TaskPath -ne '\Microsoft\Windows\Defrag\' -or $TaskName -ne 'ScheduledDefrag' -or @($Action).Count -ne 1) { throw 'wrong task update' }
    if (-not $req.apply) { throw 'read-only phase must not update the task' }
    if ($req.scenario -eq 'denied') { throw 'fixture permission denied' }
    $arguments = [string]$Action[0].Arguments
    if ($req.scenario -eq 'dropped') { $arguments = '-c -h -o -$' }
    $script:fixtureTask = [pscustomobject]@{State='Ready';Actions=@([pscustomobject]@{Execute='%windir%\system32\defrag.exe';Arguments=$arguments;WorkingDirectory=''})}
}
"#;

    fn worker(scenario: &str, apply: bool) -> Result<serde_json::Value> {
        process::powershell(
            &format!("{MOCKS}\n{SCHEDULE}"),
            &json!({"physical":r"\\.\PhysicalDrive999","scenario":scenario,"apply":apply,
                "diskId":if scenario == "identity-changed" { "different-disk" } else { "scratch-disk" },
                "volumes":[CHILD,OTHER],"before":if scenario == "concurrent" { "old arguments" } else { "-c -h -o -$" },
                "execute":r"%windir%\system32\defrag.exe","directory":"",
                "arguments":format!("-e -h -o -$ {CHILD} {OTHER}")}),
        )
    }

    #[test]
    fn schedule_worker_targets_only_child_guids_and_verifies_the_task_write() {
        let snapshot = worker("normal", false).unwrap();
        assert_eq!(snapshot["volumes"], json!([CHILD, OTHER]));
        assert_eq!(snapshot["arguments"], "-c -h -o -$");
        let updated = worker("normal", true).unwrap();
        assert_eq!(updated["arguments"], format!("-e -h -o -$ {CHILD} {OTHER}"));
        for scenario in [
            "boot",
            "host-path",
            "running",
            "custom",
            "identity-changed",
            "concurrent",
            "denied",
            "dropped",
        ] {
            assert!(
                worker(scenario, true).is_err(),
                "unsafe or failed update accepted: {scenario}"
            );
        }
    }
}
