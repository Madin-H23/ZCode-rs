//! 账本记录：一棵树一个空文件，字段都编码在文件名里（docs/specs/rust-tool-process-reaping.md 2.1）。
//!
//! `<用途>-<根 pid>-<根启动时间>-<所有者 pid>-<所有者启动时间>-<实例>`。`create_new` 一次创建，
//! 目录项原子出现，读取方不会看到写了一半的记录。

use std::io;
use std::path::{Path, PathBuf};

use super::Purpose;
use super::identity::{Process, Start};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Record {
    pub(super) purpose: Purpose,
    pub(super) root: Process,
    pub(super) owner: Process,
    pub(super) instance: u64,
}

impl Record {
    pub(super) fn name(&self) -> String {
        format!(
            "{}-{}-{}-{}-{}-{}",
            self.purpose.as_str(),
            self.root.pid,
            self.root.start.as_str(),
            self.owner.pid,
            self.owner.start.as_str(),
            self.instance
        )
    }

    /// 文件名不是本模块写出的格式时为 `None`（清扫忽略这样的条目）。
    pub(super) fn parse(name: &str) -> Option<Self> {
        let mut fields = name.split('-');
        let purpose = Purpose::parse(fields.next()?)?;
        let mut process = || -> Option<Process> {
            let pid = fields.next()?.parse().ok().filter(|pid| *pid > 1)?;
            let start = Start::parse(fields.next()?)?;
            Some(Process { pid, start })
        };
        let (root, owner) = (process()?, process()?);
        let instance = fields.next()?.parse().ok()?;
        if fields.next().is_some() {
            return None;
        }
        let record = Self {
            purpose,
            root,
            owner,
            instance,
        };
        // 只接受规范写法（例如不接受 `+12`、前导零），读到的记录与写出的文件名一一对应。
        (record.name() == name).then_some(record)
    }
}

/// 在 `dir` 下创建记录；目录不存在时先创建。同名记录已存在（同一进程不会出现）时视为成功。
pub(super) async fn create(dir: &Path, name: &str) -> io::Result<PathBuf> {
    let path = dir.join(name);
    let created = match create_new(&path).await {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            tokio::fs::create_dir_all(dir).await?;
            create_new(&path).await
        }
        other => other,
    };
    match created {
        Err(error) if error.kind() != io::ErrorKind::AlreadyExists => Err(error),
        Ok(()) | Err(_) => Ok(path),
    }
}

async fn create_new(path: &Path) -> io::Result<()> {
    tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .await
        .map(drop)
}

/// 删除记录；已经不存在（另一个清扫删掉了）时视为成功。
pub(super) async fn remove(path: &Path) -> io::Result<()> {
    match tokio::fs::remove_file(path).await {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}
