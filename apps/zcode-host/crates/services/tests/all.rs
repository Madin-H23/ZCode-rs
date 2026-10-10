//! 这个 crate 唯一的集成测试二进制（docs/specs/rust-engineering-practices.md §4.2）：每个测试文件是
//! `suite/` 下的一个模块，整个测试集只链接一次。新测试加在 `suite/` 里，不在 `tests/` 下新建
//! `*.rs`（那样 cargo 会再生成一个链接全部 Host 服务与进程内 runtime 的二进制）。
mod suite;
