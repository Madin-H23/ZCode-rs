//! 这个 crate 唯一的集成测试二进制（docs/specs/rust-engineering-practices.md §4.2）：每个测试文件是
//! `suite/` 下的一个模块。新测试加在 `suite/` 里，不在 `tests/` 下新建 `*.rs`。
mod suite;
