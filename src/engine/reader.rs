//! ChunkedReader:把一个文件的若干 Telegram 分块拼成连续的 AsyncRead + AsyncSeek
//!
//! seek 只移动游标并丢弃当前流,下次读取时按新位置重新打开对应分块
//! (块内偏移由 TgApi::open_chunk_stream 负责:local 直读 seek / HTTP Range / 丢前缀)。

use crate::db::Chunk;
use crate::error::AppResult;
use crate::telegram::{ByteReader, TgApi};
use futures_util::future::BoxFuture;
use futures_util::FutureExt;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncSeek, ReadBuf};

struct ChunkPos {
    start: u64,
    size: u64,
    file_id: String,
}

enum State {
    Idle,
    Opening { fut: BoxFuture<'static, AppResult<ByteReader>>, remain: u64 },
    Reading { reader: ByteReader, remain: u64 },
}

pub struct ChunkedReader {
    tg: Arc<dyn TgApi>,
    chunks: Vec<ChunkPos>,
    total: u64,
    pos: u64,
    state: State,
}

impl ChunkedReader {
    pub fn new(tg: Arc<dyn TgApi>, chunks: &[Chunk], start_pos: u64) -> Self {
        let mut list = Vec::with_capacity(chunks.len());
        let mut off = 0u64;
        for c in chunks {
            list.push(ChunkPos { start: off, size: c.size as u64, file_id: c.tg_file_id.clone() });
            off += c.size as u64;
        }
        ChunkedReader { tg, chunks: list, total: off, pos: start_pos.min(off), state: State::Idle }
    }

    pub fn total_size(&self) -> u64 {
        self.total
    }

    /// 当前位置所在分块的下标
    fn locate(&self, pos: u64) -> usize {
        self.chunks.partition_point(|c| c.start + c.size <= pos)
    }
}

impl AsyncRead for ChunkedReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        loop {
            if self.pos >= self.total {
                return Poll::Ready(Ok(())); // EOF
            }
            match &mut self.state {
                State::Idle => {
                    let ci = self.locate(self.pos);
                    let chunk = &self.chunks[ci];
                    let in_off = self.pos - chunk.start;
                    let remain = chunk.size - in_off;
                    let tg = self.tg.clone();
                    let fid = chunk.file_id.clone();
                    let fut = async move { tg.open_chunk_stream(&fid, in_off).await }.boxed();
                    self.state = State::Opening { fut, remain };
                }
                State::Opening { fut, remain } => match fut.as_mut().poll(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Err(e)) => {
                        self.state = State::Idle;
                        return Poll::Ready(Err(io::Error::other(e.to_string())));
                    }
                    Poll::Ready(Ok(reader)) => {
                        let remain = *remain;
                        self.state = State::Reading { reader, remain };
                    }
                },
                State::Reading { reader, remain } => {
                    let before = buf.filled().len();
                    match reader.as_mut().poll_read(cx, buf) {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                        Poll::Ready(Ok(())) => {
                            let n = (buf.filled().len() - before) as u64;
                            if n == 0 {
                                // 底层流提前结束
                                return Poll::Ready(Err(io::Error::new(
                                    io::ErrorKind::UnexpectedEof,
                                    format!("分块提前结束,还差 {remain} 字节"),
                                )));
                            }
                            if n > *remain {
                                return Poll::Ready(Err(io::Error::other(
                                    "分块返回了超出预期长度的数据",
                                )));
                            }
                            *remain -= n;
                            if *remain == 0 {
                                self.state = State::Idle; // 下次读打开下一分块
                            }
                            self.pos += n;
                            return Poll::Ready(Ok(()));
                        }
                    }
                }
            }
        }
    }
}

impl AsyncSeek for ChunkedReader {
    fn start_seek(mut self: Pin<&mut Self>, position: io::SeekFrom) -> io::Result<()> {
        let new: i128 = match position {
            io::SeekFrom::Start(p) => p as i128,
            io::SeekFrom::Current(d) => self.pos as i128 + d as i128,
            io::SeekFrom::End(d) => self.total as i128 + d as i128,
        };
        if new < 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "seek 到负位置"));
        }
        // 超过文件尾按 EOF 处理(读返回 0 字节)
        self.pos = (new as u64).min(self.total);
        self.state = State::Idle;
        Ok(())
    }

    fn poll_complete(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<u64>> {
        Poll::Ready(Ok(self.pos))
    }
}
