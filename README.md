# brz-io

基于 `ds::EphemeralBytesArena` 的分片内存字节流。Cargo 包名为 `brz-io`，使用方通过
`brz-io` 引入，Rust 中使用 `brz_io`。

| 类型 | 标准同步 trait | Tokio 异步 trait（默认启用） |
| --- | --- | --- |
| `Writer` | `std::io::Write` | `tokio::io::AsyncWrite` |
| `Reader` | `std::io::Read`、`std::io::BufRead` | `tokio::io::AsyncRead`、`tokio::io::AsyncBufRead` |

同一对象的同步和异步操作共享数据和游标。内存操作立即完成，异步实现返回
`Poll::Ready`，无需后台任务、共享管道或额外 Stream 包装类型。

## 引入

首次发布成功后，从 crates.io 引入：

```toml
[dependencies]
brz-io = "0.0.2"
brz-ds = { package = "brz-ds", version = "0.0.3", default-features = false }
```

纯同步使用方可以对 `brz-io` 设置 `default-features = false`，关闭 Tokio 依赖。

## 写入并读取

```rust
use brz_ds::EphemeralBytesArena;
use brz_io::Writer;
use std::io::{Read, Write};

let arena = EphemeralBytesArena::new(64 * 1024);
let mut writer = Writer::new(&arena);
writer.write_all(b"hello ")?;
writer.write_all(b"world")?;

let mut reader = writer.into_reader();
let mut text = String::new();
reader.read_to_string(&mut text)?;
assert_eq!(text, "hello world");
```

`Writer::new(&arena)` 无需指定总长度，按需申请分片：首片 2 KiB，随后按
4、8、16、32、64 KiB 翻倍增长，达到 64 KiB 后保持这个大小。空写入不分配；小块写入会先填满当前片，大块写入会按相同策略跨片。
分片大小不超过 arena 单个 chunk 的容量。需要总字节上限时使用
`Writer::with_limit(&arena, max_bytes)`，分片分配同时受剩余预算限制。
arena 无空间时沿用其堆回退行为。`into_reader()` 转移内存所有权，不复制 payload。

`Read` 将数据直接从各片复制到调用方的目标缓冲；`BufRead::fill_buf` 返回当前片的
借用视图，不合并所有分片。标准 `read_exact`、`read_line`、`read_until` 和
`read_to_string` 可以跨片使用，包括 UTF-8 字符跨片的情况。

Reader 持有 allocation ticket，可以比外部 arena 句柄存活更久；每个分片完全消费后
立即释放自己的 ticket。arena 仍需等待同一冻结 chunk 的其他 allocation 都释放后
才能整块复用。

## 从流读取到 EOF 或上限

```rust
use brz_ds::EphemeralBytesArena;
use brz_io::Writer;
use std::io::{self, Read};

let arena = EphemeralBytesArena::new(64 * 1024);
let max_bytes = 5;
let mut source = io::Cursor::new(b"hello world");
let mut writer = Writer::with_limit(&arena, max_bytes);
let copied = io::copy(
    &mut source.by_ref().take(max_bytes as u64),
    &mut writer,
)?;
assert_eq!(copied, 5);
assert_eq!(source.position(), 5); // 后续字节没有被消费。
let reader = writer.into_reader();
```

`take` 保证在上限处成功停止，且不额外读取一个字节探测 EOF。只有 Writer 上限、
没有 `take` 时，超出的写入遵循标准短写语义：接受剩余额度，满后 `write` 返回
`Ok(0)`，`write_all` / `copy` 报 `WriteZero`。上游可能已被 `copy` 预读，因此需要
准确限制输入消费量时应使用 `take`。向已有内容的 Writer 继续 copy 时，使用
`writer.remaining()` 作为本次输入上限。

达到上限只说明已取得这些字节，不证明输入刚好结束。输入应当是调用方希望消费的
字节流，例如一条响应的 body；协议消息边界由调用方负责。

## 异步 IO

```rust
use brz_ds::EphemeralBytesArena;
use brz_io::Writer;
use tokio::io::{self, AsyncReadExt};

let arena = EphemeralBytesArena::new(64 * 1024);
let max_bytes = 1024;
let source = &b"hello world"[..]; // 也可以是实现 AsyncRead 的 socket/body。
let mut writer = Writer::with_limit(&arena, max_bytes);
io::copy(&mut source.take(max_bytes as u64), &mut writer).await?;

let mut reader = writer.into_reader();
let mut destination = Vec::new(); // 也可以是实现 AsyncWrite 的 socket/file。
io::copy(&mut reader, &mut destination).await?;
assert_eq!(destination, b"hello world");
```

`flush` 和异步 `shutdown` 都是内存写入的空操作，与 Tokio 的 `Vec<u8>` writer
行为一致，不会封闭 Writer。通过 `into_reader()` 完成写入并转为读取。

## 借用读取

`Reader` 还提供消费数据的 `read_bytes(len)`、`read_str(len)`、`skip(len)`，以及不推进
游标的 `peek_bytes(offset, len)` 和 `peek_byte(offset)`。`offset` 相对于当前游标，
`position()` 返回相对于原始输入的消费位置。

引用型读取使用 `&self`，可以保留多个结果并继续读取：

```rust
let reader = writer.into_reader();
let name = reader.read_str(name_len)?;
let description = reader.read_str(description_len)?;
// name 和 description 可以同时使用。
```

范围在一个分片内时直接借用；跨片时按请求长度合并到 arena。`read_str` 验证 UTF-8，
验证失败或输入不足不推进游标。共享读取期间，原始分片和合并后的所有分配都保持
存活，不覆盖临时空间。重新获得 `&mut Reader` 并执行标准 IO 消费时，可以回收已消费
分片及额外存储。`store_bytes_with` 允许格式解析器把转义解码等结果直接写入 arena，
返回的引用同样由 Reader 持有；JSON 规则由独立的 `brz-json` 项目处理。

稳定存储采用 `elsa::sync::FrozenVec` 保存 allocation owner；新增的 Box 用于固定
owner 元数据，payload 仍在 arena 中。本 crate 不包含 unsafe 代码。共享方法的单次
游标更新是原子的，多步协议解析仍应由一个解析者顺序执行。

## 验证

```sh
cargo fmt --all -- --check
cargo test --all-features
cargo test --no-default-features
cargo clippy --all-targets --all-features -- -D warnings
cargo clippy --all-targets --no-default-features -- -D warnings
```

`Reader::view()` 为当前未读范围创建借用视图。视图不随源 Reader 的共享游标推进而
改变，可供 JSON 等解析器维护自己的游标；持有视图期间，Rust 借用规则阻止独占 IO
提前回收分片。`Reader::as_slice()` 返回全部未读字节，跨片时缓存一次 arena 合并结果，
后续共享读取保留缓存，恢复独占 IO 时释放缓存。

## CI and publishing

Pushes and pull requests run rustfmt, Clippy and tests with all features and
without default features, plus release-mode tests. Cargo.lock is
tracked. This crate has no Loom models and forbids unsafe code.

The default branch is `main`. Grant this public repository access to the
`we-breeze` organization Actions secret `CARGO_REGISTRY_TOKEN`. The token must
allow creating and publishing `brz-io`. Repository rules must allow Actions to
push version commits and create tags (`contents: write`).

Use **Actions → Publish → Run workflow**, select `main`, and leave `retry_tag`
empty. Publish increments the latest `v0.0.x` tag, updates Cargo.toml and
Cargo.lock, runs checks and a publishing dry run, then atomically pushes the
version commit and annotated tag before uploading to crates.io. The existing
`v0.0.1` means the next release is `v0.0.2`, replacing the initial unpublished
Cargo version `0.1.0`. Pushes and merges only run CI; publishing is manual.

If upload fails after the tag is pushed, start a new run with `retry_tag` set
to that existing tag. This field does not choose a new version. Check crates.io
before retrying an upload timeout: published versions cannot be overwritten.
Publishing is serialized and rejects stale checkouts. Source fixes require a
new release. No GitHub Release is created.

## Operational safety

Use `Writer::with_limit` and bound the input stream when reading untrusted
payloads. `Writer::new` has no application byte limit. Borrowed cross-segment
reads and `store_bytes_with` retain additional allocations until exclusive IO
resumes or the reader is dropped; repeated peeks or failed cross-segment UTF-8
reads can therefore grow memory usage even without advancing the cursor.
Treat decoding sizes and repeated parsing attempts as application-level limits.

## License

Licensed under either the MIT license or the Apache License, Version 2.0,
at your option. See [LICENSE-MIT](LICENSE-MIT) and [LICENSE-APACHE](LICENSE-APACHE).
