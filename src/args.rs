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

use anyhow::Context;
use clap::Parser;
use std::path::PathBuf;

/// 与可执行文件同目录的默认配置文件名
const CONFIG_FILE_NAME: &str = "config.toml";

/// A cross-platform daemon for remote devices.
#[derive(Debug, Parser)]
#[command(name = "rsd", version)]
pub struct Args {
    /// Config file path (default "./config.toml")
    #[arg(short = 'c', long)]
    config: Option<PathBuf>,
}

impl Args {
    /// 解析进程启动参数
    ///
    /// 使用 clap 自带的 `parse()`：`--help` / `--version` 会直接打印并退出（退出码 0），
    /// 参数错误会打印错误与用法并退出（退出码 2），均不会返回 `Err`。
    /// 这里刻意不用 `try_parse()`：clap 的 `parse()` 会直接输出帮助/错误并退出，
    /// 无需把错误绕经 `run()` 返回给 `main` 再输出。
    pub fn parse_args() -> Self {
        Self::parse()
    }

    /// 返回最终生效的配置文件路径：显式指定优先，否则回退到可执行文件同目录
    ///
    /// 回退逻辑延迟到此处而非解析参数时执行；`current_exe()` 的失败会随配置加载
    /// 错误一起由 `main` 的 `eprintln!` 输出到 stderr。
    pub fn config(&self) -> anyhow::Result<PathBuf> {
        match &self.config {
            Some(path) => Ok(path.clone()),
            None => default_config(),
        }
    }
}

/// 解析默认配置路径：`<可执行文件所在目录>/config.toml`
///
/// 使用 `current_exe()` 而非当前工作目录，使服务以绝对路径启动时
/// （systemd / launchd / 双击运行）也能定位到随程序分发的配置文件。
/// 注意 Linux/macOS 上该调用会解析符号链接，配置取自真实二进制所在目录，
/// 而非符号链接所在目录。
fn default_config() -> anyhow::Result<PathBuf> {
    let exe = std::env::current_exe().context("Failed to resolve current executable path")?;

    let dir = exe
        .parent()
        .with_context(|| format!("Executable path has no parent directory: {}", exe.display()))?;

    Ok(dir.join(CONFIG_FILE_NAME))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    /// clap 自带的属性校验：检查 derive 生成的命令定义是否合法（重复 short/long 等）。
    #[test]
    fn args_definition_is_valid() {
        Args::command().debug_assert();
    }

    #[test]
    fn parses_short_config_flag() {
        let args = Args::try_parse_from(["rsd", "-c", "/etc/rsd/config.toml"])
            .expect("-c should be accepted");

        assert_eq!(
            args.config().expect("explicit path should be used"),
            PathBuf::from("/etc/rsd/config.toml")
        );
    }

    #[test]
    fn parses_long_config_flag() {
        let args =
            Args::try_parse_from(["rsd", "--config", "/tmp/a.toml"]).expect("--config works");

        assert_eq!(
            args.config().expect("explicit path should be used"),
            PathBuf::from("/tmp/a.toml")
        );
    }

    /// `-c` 省略时回退到可执行文件同目录（测试进程的可执行文件即测试二进制）。
    #[test]
    fn falls_back_to_exe_dir_when_omitted() {
        let args = Args::try_parse_from(["rsd"]).expect("no args should be accepted");
        assert!(args.config.is_none(), "-c 省略时字段应为 None");

        let path = args.config().expect("default path should resolve");
        assert!(path.is_absolute(), "默认路径应基于可执行文件的绝对路径");

        let exe = std::env::current_exe().expect("test executable path");
        assert_eq!(
            path,
            exe.parent()
                .expect("test exe has parent")
                .join(CONFIG_FILE_NAME)
        );
    }

    #[test]
    fn rejects_unknown_flag() {
        let error = Args::try_parse_from(["rsd", "--nope"]).expect_err("unknown flag should fail");

        assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    /// clap 直接在解析阶段报错，不会静默忽略缺值。
    /// 这里只断言 `is_err()`：clap 未给出“选项缺值”的稳定 `ErrorKind`，锁定具体变体会写出脆弱测试。
    #[test]
    fn rejects_config_flag_without_value() {
        assert!(Args::try_parse_from(["rsd", "-c"]).is_err());
    }

    /// `--version` 依赖 `#[command(version)]`，属性缺失时会退化成 UnknownArgument。
    #[test]
    fn version_flag_reports_package_version() {
        let error = Args::try_parse_from(["rsd", "--version"]).expect_err("--version exits early");

        assert_eq!(error.kind(), clap::error::ErrorKind::DisplayVersion);
        assert!(error.to_string().contains(env!("CARGO_PKG_VERSION")));
    }
}
