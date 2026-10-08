//! 多个 service 共用的系统侧工具：子进程基础环境、Node `path` 语义、Node 风格的错误文案、
//! `localeCompare` 近似排序和进程信号。
pub mod collate;
pub mod env;
pub mod node_error;
pub mod path;
pub mod signal;
