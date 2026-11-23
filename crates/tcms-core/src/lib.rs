//! 团队上下文系统的核心类型：条目模型和上下文仓库的文件格式。不碰 git，也不碰数据库。

pub mod format;
pub mod model;
pub mod teams;

pub use format::{
    ParsedFile, ParsedItem, Warning, assign_ids, clean_export_name, parse_file, scope_of_path,
};
pub use model::{ItemId, Kind, Scope, Status, utc_today, valid_date, valid_name};
pub use teams::{TEAMS_FILE, Teams};
