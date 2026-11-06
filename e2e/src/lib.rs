//! E2E ハーネス。コンテナ(`e2e/Dockerfile` のイメージ)を起動し、ホストから `exec` で操作して確かめる。
//!
//! 試験項目の台帳は `e2e/TEST-ITEMS.md`。ID(A01 など)はテスト名にそのまま使う。

pub mod harness;
pub mod image_check;
pub mod sandbox;
pub mod wait;

pub use sandbox::{Display, Runtime, Sandbox};
