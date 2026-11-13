//! atb daemon：HTTP API + 扩展 WebSocket 桥接。
//!
//! 按领域拆分的子模块，共享同一套命名空间（文件末尾统一 `pub(crate) use`）。
//! 拆分不改变调用关系，只是把代码放进可检索、可并行编辑的文件。

use crate::open_target::{resolve_open_target, OpenTargetError, TargetKind};
use crate::protocol::{
    DriverState, ElementDomInfo, ElementRef, ExecResult, RectInfo, Session, SnapshotCache, TabInfo,
    WsIncoming,
};
use crate::target_spec::TargetSpec;
use crate::{config, html};
use anyhow::{anyhow, Result};
use axum::body::Body;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{header::ORIGIN, HeaderMap, Request, StatusCode, Uri};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::hash::{Hash, Hasher};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::{mpsc, oneshot, Mutex, Notify, RwLock};
use uuid::Uuid;

pub(crate) mod action;
pub(crate) mod boot;
pub(crate) mod broadcast;
pub(crate) mod browsers;
pub(crate) mod capture;
pub(crate) mod cdp;
pub(crate) mod challenge;
pub(crate) mod challenge_click;
pub(crate) mod challenge_probe;
pub(crate) mod downloads;
pub(crate) mod dto;
pub(crate) mod env;
pub(crate) mod exec;
pub(crate) mod extension_logs;
pub(crate) mod focus;
pub(crate) mod lifecycle;
pub(crate) mod monitor;
pub(crate) mod multi;
pub(crate) mod net;
pub(crate) mod open;
pub(crate) mod page_data;
pub(crate) mod pdf; // 图片型 PDF 生成器（CDP 不可用时的整页截图回落）
pub(crate) mod read_find;
pub(crate) mod run;
pub(crate) mod sessions;
pub(crate) mod snapshot;
pub(crate) mod state;
pub(crate) mod tabs;
pub(crate) mod target_info;
pub(crate) mod targets;
pub(crate) mod tree;
pub(crate) mod ws;

#[cfg(test)]
mod tests;

pub(crate) use action::*;
pub(crate) use boot::*;
pub(crate) use broadcast::*;
pub(crate) use browsers::*;
pub(crate) use capture::*;
pub(crate) use cdp::*;
pub(crate) use challenge::*;
pub(crate) use challenge_click::*;
pub(crate) use challenge_probe::*;
pub(crate) use downloads::*;
pub(crate) use dto::*;
pub(crate) use env::*;
pub(crate) use exec::*;
pub(crate) use extension_logs::*;
pub(crate) use focus::*;
pub(crate) use lifecycle::*;
pub(crate) use monitor::*;
pub(crate) use multi::*;
pub(crate) use net::*;
pub(crate) use open::*;
pub(crate) use page_data::*;
pub(crate) use read_find::*;
pub(crate) use run::*;
pub(crate) use sessions::*;
pub(crate) use snapshot::*;
pub(crate) use state::*;
pub(crate) use tabs::*;
pub(crate) use target_info::*;
pub(crate) use targets::*;
pub(crate) use tree::*;
pub(crate) use ws::*;
