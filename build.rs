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

//! 构建脚本：把仓库内的 `config/config.toml` 复制到产物目录。
//!
//! 程序默认读取可执行文件同目录下的 `config.toml`，而构建产物位于
//! `target/<profile>/`，仓库里的源配置不在那里。复制一份即可让本地
//! `cargo run`（不带 `-c`）开箱可用。
//!
//! 注意：本脚本只在输入变化时重新执行。若手动删除了产物目录下的
//! `config.toml`，需要 `touch config/config.toml` 或 `cargo clean -p rsd`
//! 才能触发重新复制。

use std::env;
use std::fs;
use std::path::Path;

/// 仓库内的源配置文件（相对包根目录）
const CONFIG_SRC: &str = "config/config.toml";
/// 复制到产物目录后的文件名，需与 `src/args.rs` 的 `CONFIG_FILE_NAME` 保持一致
const CONFIG_DST: &str = "config.toml";

fn main() {
    // 源配置变化时重新执行本脚本，使改动能同步到产物目录
    println!("cargo:rerun-if-changed={CONFIG_SRC}");

    let out_dir = env::var("OUT_DIR").expect("OUT_DIR is set for build scripts");

    // OUT_DIR 布局为 target/<profile>/build/<crate>-<hash>/out，
    // 向上 3 层即 target/<profile>，正是可执行文件所在目录；
    // 指定 --target 时该布局同样成立。
    // 该推算并非 Cargo 官方保证，若将来布局调整，此处会 panic 并以构建错误暴露。
    let profile_dir = Path::new(&out_dir)
        .ancestors()
        .nth(3)
        .expect("OUT_DIR should be target/<profile>/build/<crate>-<hash>/out");

    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join(CONFIG_SRC);
    let dst = profile_dir.join(CONFIG_DST);

    fs::copy(&src, &dst).unwrap_or_else(|error| {
        panic!(
            "Failed to copy {} to {}: {error}",
            src.display(),
            dst.display()
        )
    });
}
