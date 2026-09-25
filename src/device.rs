// Copyright 2026 The rsd Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::network::protocol::DeviceInfo;
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
#[cfg(target_os = "linux")]
use std::fs;
#[cfg(any(windows, target_os = "macos"))]
use std::process::Command;

/// 构建 declare 消息所需的设备信息。
///
/// `device_id` 的原始标识按固定优先级采集：
/// - Windows：SMBIOS UUID → MachineGuid → 硬件指纹（磁盘序列号 + 主网卡 MAC）
/// - Linux：SMBIOS UUID → machine-id → 硬件指纹（磁盘序列号 + 主网卡 MAC）
/// - macOS：IOPlatformUUID（硬件 UUID）→ IOPlatformSerialNumber（整机序列号）→ 硬件指纹（主网卡 MAC）
///
/// `device_id` 为原始标识的 SHA-256（大写 hex）截取前 16 位，见 `device_id_from`。
///
/// 所有来源都取不到时返回错误，由调用方按启动失败处理（不使用 hostname 兜底）。
pub fn device_info() -> Result<DeviceInfo> {
    let raw = raw_identifier().context("Failed to collect any device identifier source")?;
    Ok(DeviceInfo {
        device_id: device_id_from(&raw),
        ip: local_ip(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    })
}

/// 按平台优先级取第一个可用的原始标识。
fn raw_identifier() -> Option<String> {
    let sources: Vec<Option<String>> = {
        #[cfg(windows)]
        {
            vec![
                windows_smbios_uuid(),
                windows_machine_guid(),
                windows_fingerprint(),
            ]
        }
        #[cfg(target_os = "linux")]
        {
            vec![linux_smbios_uuid(), linux_machine_id(), linux_fingerprint()]
        }
        #[cfg(target_os = "macos")]
        {
            vec![
                macos_io_platform_uuid(),
                macos_serial_number(),
                macos_fingerprint(),
            ]
        }
        #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
        {
            vec![]
        }
    };
    first_nonempty(sources)
}

/// 从候选来源中取第一个有值者（忽略空白字符串）。
fn first_nonempty(sources: Vec<Option<String>>) -> Option<String> {
    sources.into_iter().flatten().find(|s| !s.trim().is_empty())
}

/// device_id 的十六进制长度：SHA-256 截取前 16 位（64 bit 熵）。
const DEVICE_ID_HEX_LEN: usize = 16;

/// 生成短设备 ID：SHA-256 大写 hex 截取前 16 位。
fn device_id_from(raw: &str) -> String {
    sha256_hex(raw).chars().take(DEVICE_ID_HEX_LEN).collect()
}

/// 对原始标识做 SHA-256，输出 64 位大写十六进制字符串。
fn sha256_hex(input: &str) -> String {
    let digest = Sha256::digest(input.as_bytes());
    digest.iter().map(|b| format!("{b:02X}")).collect()
}

/// 探测本机 IP：通过 UDP 套接字让系统选择出网路由，不产生实际网络流量。
fn local_ip() -> String {
    let ip = std::net::UdpSocket::bind("0.0.0.0:0")
        .ok()
        .and_then(|socket| {
            socket.connect("8.8.8.8:80").ok()?;
            socket.local_addr().ok().map(|addr| addr.ip().to_string())
        });
    ip.unwrap_or_else(|| "127.0.0.1".to_string())
}

/// 校验 UUID 文本：空、或全零（如未配置的虚拟机）视为无效。
#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
fn valid_uuid(text: &str) -> Option<String> {
    let uuid = text.trim();
    if uuid.is_empty() {
        return None;
    }
    let compact: String = uuid.chars().filter(|c| *c != '-').collect();
    if !compact.is_empty() && compact.chars().all(|c| c == '0') {
        return None;
    }
    Some(uuid.to_string())
}

/// 取文本中第一个非空行。
#[cfg(windows)]
fn first_line(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_string)
}

/// 组合硬件指纹：仅拼接非空的磁盘序列号与主网卡 MAC，全部为空时返回 None。
#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
fn build_fingerprint(disk: String, mac: String) -> Option<String> {
    let mut parts = Vec::new();
    if !disk.is_empty() {
        parts.push(format!("disk:{disk}"));
    }
    if !mac.is_empty() {
        parts.push(format!("mac:{mac}"));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("|"))
    }
}

/// 依次读取多个文件，取第一个非空内容。
#[cfg(target_os = "linux")]
fn read_first_nonempty(paths: &[&str]) -> Option<String> {
    for path in paths {
        if let Ok(content) = fs::read_to_string(path) {
            let value = content.trim().to_string();
            if !value.is_empty() {
                return Some(value);
            }
        }
    }
    None
}

#[cfg(windows)]
fn windows_smbios_uuid() -> Option<String> {
    let out = run_powershell(
        "[Console]::OutputEncoding=[System.Text.Encoding]::UTF8;(Get-CimInstance Win32_ComputerSystemProduct | Select-Object -First 1 -ExpandProperty UUID)",
    )?;
    let uuid = first_line(&out)?;
    valid_uuid(&uuid)
}

#[cfg(windows)]
fn windows_machine_guid() -> Option<String> {
    let out = Command::new("reg")
        .args([
            "query",
            r"HKLM\SOFTWARE\Microsoft\Cryptography",
            "/v",
            "MachineGuid",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .find(|line| line.contains("MachineGuid"))
        .and_then(|line| line.split_whitespace().last().map(str::to_string))
        .filter(|s| !s.is_empty())
}

#[cfg(windows)]
fn windows_fingerprint() -> Option<String> {
    let script = concat!(
        "[Console]::OutputEncoding=[System.Text.Encoding]::UTF8;",
        "$disk=(Get-CimInstance Win32_DiskDrive | Where-Object {$_.SerialNumber} | Select-Object -First 1 -ExpandProperty SerialNumber);",
        "$mac=(Get-CimInstance Win32_NetworkAdapter | Where-Object {$_.PhysicalAdapter -eq $true -and $_.NetConnectionStatus -eq 2} | Select-Object -First 1 -ExpandProperty MACAddress);",
        "[pscustomobject]@{disk=[string]$disk;mac=[string]$mac} | ConvertTo-Json -Compress"
    );
    let out = run_powershell(script)?;
    let value: serde_json::Value = serde_json::from_str(out.trim()).ok()?;
    let disk = value
        .get("disk")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let mac = value
        .get("mac")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    build_fingerprint(disk, mac)
}

#[cfg(windows)]
fn run_powershell(script: &str) -> Option<String> {
    let out = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(target_os = "linux")]
fn linux_smbios_uuid() -> Option<String> {
    let content = fs::read_to_string("/sys/class/dmi/id/product_uuid").ok()?;
    valid_uuid(&content)
}

#[cfg(target_os = "linux")]
fn linux_machine_id() -> Option<String> {
    read_first_nonempty(&["/etc/machine-id", "/var/lib/dbus/machine-id"])
}

#[cfg(target_os = "linux")]
fn linux_fingerprint() -> Option<String> {
    let disk = first_disk_serial();
    let mac = primary_mac();
    build_fingerprint(disk.unwrap_or_default(), mac.unwrap_or_default())
}

#[cfg(target_os = "linux")]
fn primary_mac() -> Option<String> {
    let entries = fs::read_dir("/sys/class/net").ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == "lo" {
            continue;
        }
        // 物理接口在 /sys/class/net/<iface>/device 下有符号链接，虚拟接口没有
        if fs::symlink_metadata(entry.path().join("device")).is_err() {
            continue;
        }
        if let Ok(address) = fs::read_to_string(entry.path().join("address")) {
            let mac = address.trim().to_string();
            if !mac.is_empty() && mac != "00:00:00:00:00:00" {
                return Some(mac);
            }
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn first_disk_serial() -> Option<String> {
    let entries = fs::read_dir("/sys/block").ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("loop") || name.starts_with("ram") {
            continue;
        }
        if let Ok(serial) = fs::read_to_string(entry.path().join("device").join("serial")) {
            let serial = serial.trim().to_string();
            if !serial.is_empty() {
                return Some(serial);
            }
        }
    }
    None
}

#[cfg(target_os = "macos")]
fn macos_io_platform_uuid() -> Option<String> {
    let out = run_macos_cmd("ioreg", &["-rd1", "-c", "IOPlatformExpertDevice"])?;
    let uuid = macos_ioreg_value(&out, "IOPlatformUUID")?;
    valid_uuid(&uuid)
}

#[cfg(target_os = "macos")]
fn macos_serial_number() -> Option<String> {
    let out = run_macos_cmd("ioreg", &["-rd1", "-c", "IOPlatformExpertDevice"])?;
    macos_ioreg_value(&out, "IOPlatformSerialNumber")
}

#[cfg(target_os = "macos")]
fn macos_fingerprint() -> Option<String> {
    let out = run_macos_cmd("ifconfig", &[])?;
    let mac = macos_primary_mac(&out)?;
    build_fingerprint(String::new(), mac)
}

/// 执行命令，成功时返回 stdout。
#[cfg(target_os = "macos")]
fn run_macos_cmd(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// 从 `ioreg` 输出中取指定键的字符串值（形如 `"IOPlatformUUID" = "..."`）。
#[cfg(target_os = "macos")]
fn macos_ioreg_value(output: &str, key: &str) -> Option<String> {
    output
        .lines()
        .find(|line| line.contains(key))
        .and_then(|line| line.split('"').nth(3).map(str::to_string))
        .filter(|s| !s.trim().is_empty())
}

/// 取主网卡 MAC：跳过 lo0，跳过组播/本地管理地址（虚拟接口，如 anpi*、en1-6、awdl0）。
#[cfg(target_os = "macos")]
fn macos_primary_mac(output: &str) -> Option<String> {
    let mut iface = String::new();
    for line in output.lines() {
        // 顶格行是接口头，形如 "en0: flags=..."，更新当前接口名
        if line.starts_with(|c: char| !c.is_whitespace()) {
            if let Some(name) = line.split(':').next() {
                iface = name.to_string();
            }
            continue;
        }
        if iface != "lo0" {
            if let Some(mac) = line.trim().strip_prefix("ether ") {
                let mac = mac.trim();
                if macos_is_unicast_mac(mac) {
                    return Some(mac.to_string());
                }
            }
        }
    }
    None
}

/// 校验 MAC 为全局唯一单播地址：首字节最低两位（组播位 + 本地管理位）必须为 0。
#[cfg(target_os = "macos")]
fn macos_is_unicast_mac(mac: &str) -> bool {
    let first = mac
        .split(':')
        .next()
        .and_then(|s| u8::from_str_radix(s, 16).ok());
    matches!(first, Some(octet) if octet & 0x03 == 0) && mac != "00:00:00:00:00:00"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_hex_is_64_chars() {
        let digest = sha256_hex("test");
        assert_eq!(digest.len(), 64);
        assert!(digest.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn sha256_hex_matches_known_value() {
        // "hello" 的标准 SHA-256（大写十六进制）
        assert_eq!(
            sha256_hex("hello"),
            "2CF24DBA5FB0A30E26E83B2AC5B9E29E1B161E5C1FA7425E73043362938B9824"
        );
    }

    #[test]
    fn device_id_is_16_uppercase_hex_and_prefix_of_full_hash() {
        let full = sha256_hex("test");
        let id = device_id_from("test");
        assert_eq!(id.len(), DEVICE_ID_HEX_LEN);
        assert!(full.starts_with(&id));
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(id, id.to_uppercase());
    }

    #[test]
    fn first_nonempty_takes_first_available() {
        assert_eq!(
            first_nonempty(vec![None, Some("   ".to_string()), Some("b".to_string())]),
            Some("b".to_string())
        );
        assert_eq!(first_nonempty(vec![None, None]), None);
    }

    #[test]
    #[cfg(any(windows, target_os = "linux", target_os = "macos"))]
    fn valid_uuid_rejects_empty_and_all_zero() {
        assert!(valid_uuid("   ").is_none());
        assert!(valid_uuid("00000000-0000-0000-0000-000000000000").is_none());
        assert_eq!(
            valid_uuid("8D8C0B44-0000-0000-0000-000000000001").unwrap(),
            "8D8C0B44-0000-0000-0000-000000000001"
        );
    }

    #[test]
    #[cfg(any(windows, target_os = "linux", target_os = "macos"))]
    fn fingerprint_skips_empty_parts() {
        assert_eq!(
            build_fingerprint(String::new(), "aa:bb:cc:dd:ee:ff".into()),
            Some("mac:aa:bb:cc:dd:ee:ff".to_string())
        );
        assert_eq!(
            build_fingerprint("sda123".into(), String::new()),
            Some("disk:sda123".to_string())
        );
        assert_eq!(build_fingerprint(String::new(), String::new()), None);
    }

    #[test]
    fn local_ip_is_never_empty() {
        assert!(!local_ip().is_empty());
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn macos_ioreg_value_parses_quoted_value() {
        let out = "    \"IOPlatformUUID\" = \"AEDE46AB-B706-5A50-B04D-78DE5545D206\"\n";
        assert_eq!(
            macos_ioreg_value(out, "IOPlatformUUID").as_deref(),
            Some("AEDE46AB-B706-5A50-B04D-78DE5545D206")
        );
        assert_eq!(macos_ioreg_value(out, "IOPlatformSerialNumber"), None);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn macos_primary_mac_skips_virtual_interfaces() {
        let out = concat!(
            "lo0: flags=8049<UP,LOOPBACK,RUNNING,MULTICAST> mtu 16384\n",
            "anpi0: flags=8863<UP,BROADCAST,SMART,RUNNING,SIMPLEX,MULTICAST> mtu 1500\n",
            "\tether fe:f0:37:e4:84:43\n",
            "ap1: flags=8863<UP,BROADCAST,SMART,RUNNING,SIMPLEX,MULTICAST> mtu 1500\n",
            "\tether 6e:b1:33:a1:fa:f3\n",
            "en0: flags=8863<UP,BROADCAST,SMART,RUNNING,SIMPLEX,MULTICAST> mtu 1500\n",
            "\tether 6c:b1:33:a1:fa:f3\n",
        );
        assert_eq!(macos_primary_mac(out).as_deref(), Some("6c:b1:33:a1:fa:f3"));
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn macos_is_unicast_mac_filters_virtual() {
        assert!(macos_is_unicast_mac("6c:b1:33:a1:fa:f3"));
        assert!(!macos_is_unicast_mac("fe:f0:37:e4:84:43"));
        assert!(!macos_is_unicast_mac("36:5c:e4:99:5c:40"));
        assert!(!macos_is_unicast_mac("00:00:00:00:00:00"));
        assert!(!macos_is_unicast_mac(""));
    }
}
