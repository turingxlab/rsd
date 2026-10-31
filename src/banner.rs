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

/// 启动横幅：宽体 "rsd" 字符画。
const BANNER: &str = r#"
██████╗ ███████╗██████╗
██╔══██╗██╔════╝██╔══██╗
██████╔╝███████╗██║  ██║
██╔══██╗╚════██║██║  ██║
██║  ██║███████║██████╔╝
╚═╝  ╚═╝╚══════╝╚═════╝
"#;

/// 原样输出启动横幅，不经过日志级别过滤。
pub fn print_banner(writer: &mut impl std::io::Write) -> std::io::Result<()> {
    // 一次提交完整横幅，避免后台写入时与其他日志交错。
    writer.write_all(format!("{}\nrsd v{}\n\n", BANNER, env!("CARGO_PKG_VERSION")).as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_banner_version_and_spacing() {
        let mut output = Vec::new();
        print_banner(&mut output).unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            format!("{BANNER}\nrsd v{}\n\n", env!("CARGO_PKG_VERSION"))
        );
    }

    #[test]
    fn propagates_write_errors() {
        struct FailingWriter;
        impl std::io::Write for FailingWriter {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("write failed"))
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        assert!(print_banner(&mut FailingWriter).is_err());
    }
}
