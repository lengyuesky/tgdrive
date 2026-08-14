//! 从视频文件头解析显示宽高与时长,发 sendVideo 时带上,
//! 否则 Telegram 不探测文件内容,消息气泡的长宽比是猜的。
//!
//! 只支持 mp4 家族(mp4/m4v/mov,ISO-BMFF box 结构),与视频消息模式的
//! 范围一致 —— Telegram 客户端仅保证播放 MPEG-4 视频,其他容器仍走文档。
//! 任何一步解析不了都返回 None,调用方按无元数据发送(行为同旧版)。

use crate::telegram::{UploadBody, VideoMeta};
use std::io::{Read, Seek, SeekFrom};

/// moov 一般只有几 MB;超过该值视为异常文件,放弃解析
const MOOV_MAX: u64 = 64 * 1024 * 1024;

/// 从上传体解析视频元数据(内存块直接读,临时文件重开一个句柄)
pub fn probe_body(mime: &str, body: &UploadBody) -> Option<VideoMeta> {
    match body {
        UploadBody::Memory(b) => probe_video(mime, &mut std::io::Cursor::new(&b[..])),
        UploadBody::TempFile { file, .. } => {
            let f = file.reopen().ok()?;
            probe_video(mime, &mut std::io::BufReader::new(f))
        }
    }
}

pub fn probe_video(mime: &str, r: &mut (impl Read + Seek)) -> Option<VideoMeta> {
    let meta = match mime {
        "video/mp4" | "video/x-m4v" | "video/quicktime" => probe_mp4(r),
        _ => None,
    };
    // 异常尺寸不如不带:Telegram 对 0 或天文数字会直接 400
    meta.filter(|m| (1..=10_000).contains(&m.width) && (1..=10_000).contains(&m.height))
}

// ---------- ISO-BMFF(mp4 / m4v / mov) ----------

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes(b[..4].try_into().unwrap())
}

fn be64(b: &[u8]) -> u64 {
    u64::from_be_bytes(b[..8].try_into().unwrap())
}

/// 顶层顺序扫 box(mdat 可能几 GB,用 seek 跳过),找到 moov 整体读进内存再解析
fn probe_mp4(r: &mut (impl Read + Seek)) -> Option<VideoMeta> {
    let file_end = r.seek(SeekFrom::End(0)).ok()?;
    let mut pos = 0u64;
    while pos + 8 <= file_end {
        r.seek(SeekFrom::Start(pos)).ok()?;
        let mut hdr = [0u8; 8];
        r.read_exact(&mut hdr).ok()?;
        let typ = [hdr[4], hdr[5], hdr[6], hdr[7]];
        let (size, hdr_len) = match be32(&hdr) {
            0 => (file_end - pos, 8u64), // size=0:延伸到文件尾
            1 => {
                let mut b = [0u8; 8];
                r.read_exact(&mut b).ok()?;
                (u64::from_be_bytes(b), 16u64)
            }
            s => (s as u64, 8u64),
        };
        if size < hdr_len || pos + size > file_end {
            return None;
        }
        if &typ == b"moov" {
            let payload = size - hdr_len;
            if payload > MOOV_MAX {
                return None;
            }
            let mut buf = vec![0u8; payload as usize];
            r.read_exact(&mut buf).ok()?;
            return parse_moov(&buf);
        }
        pos += size;
    }
    None
}

/// 遍历一段缓冲里的兄弟 box
fn walk_boxes<'a>(buf: &'a [u8], mut f: impl FnMut(&[u8; 4], &'a [u8])) {
    let mut off = 0usize;
    while off + 8 <= buf.len() {
        let typ = [buf[off + 4], buf[off + 5], buf[off + 6], buf[off + 7]];
        let (size, hl) = match be32(&buf[off..]) {
            0 => (buf.len() - off, 8),
            1 => {
                if off + 16 > buf.len() {
                    return;
                }
                let s = be64(&buf[off + 8..]);
                if s > buf.len() as u64 {
                    return;
                }
                (s as usize, 16)
            }
            s => (s as usize, 8),
        };
        if size < hl || off + size > buf.len() {
            return;
        }
        f(&typ, &buf[off + hl..off + size]);
        off += size;
    }
}

fn parse_moov(moov: &[u8]) -> Option<VideoMeta> {
    let mut duration_secs = None;
    let mut dims: Option<(u32, u32)> = None;
    walk_boxes(moov, |typ, payload| match typ {
        b"mvhd" => duration_secs = parse_mvhd(payload),
        b"trak" if dims.is_none() => {
            walk_boxes(payload, |t2, p2| {
                if t2 == b"tkhd" && dims.is_none() {
                    dims = parse_tkhd(p2);
                }
            });
        }
        _ => {}
    });
    let (width, height) = dims?;
    Some(VideoMeta { width, height, duration_secs })
}

/// mvhd:timescale + duration → 秒
fn parse_mvhd(p: &[u8]) -> Option<u32> {
    let (timescale, duration) = match p.first()? {
        0 if p.len() >= 20 => (be32(&p[12..]) as u64, be32(&p[16..]) as u64),
        1 if p.len() >= 32 => (be32(&p[20..]) as u64, be64(&p[24..])),
        _ => return None,
    };
    // 全 1 表示时长未知
    if timescale == 0 || duration == 0 || duration == u32::MAX as u64 || duration == u64::MAX {
        return None;
    }
    u32::try_from(duration / timescale).ok()
}

/// tkhd:宽高为 16.16 定点;手机竖拍视频宽高是编码尺寸,旋转在变换矩阵里 → 按矩阵交换
fn parse_tkhd(p: &[u8]) -> Option<(u32, u32)> {
    let matrix_off = match p.first()? {
        0 => 4 + 20 + 16,
        1 => 4 + 32 + 16,
        _ => return None,
    };
    if p.len() < matrix_off + 44 {
        return None;
    }
    // 矩阵 [a b u c d v x y w]:a=0 且 d=0 即 90°/270° 旋转
    let a = be32(&p[matrix_off..]);
    let d = be32(&p[matrix_off + 16..]);
    let w = be32(&p[matrix_off + 36..]) >> 16;
    let h = be32(&p[matrix_off + 40..]) >> 16;
    if w == 0 || h == 0 {
        return None; // 音频轨等宽高为 0,跳过
    }
    Some(if a == 0 && d == 0 { (h, w) } else { (w, h) })
}

/// 测试用的最小 mp4 合成工具(probe 单测与上传管线集成测试共用)
#[cfg(test)]
pub mod testutil {
    fn bx(typ: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut v = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(typ);
        v.extend_from_slice(payload);
        v
    }

    fn mvhd(timescale: u32, duration: u32) -> Vec<u8> {
        let mut p = vec![0u8; 12]; // version+flags, creation, modification
        p.extend_from_slice(&timescale.to_be_bytes());
        p.extend_from_slice(&duration.to_be_bytes());
        p.extend_from_slice(&[0u8; 80]); // rate/volume/matrix 等占位
        bx(b"mvhd", &p)
    }

    fn tkhd(w: u32, h: u32, rotated: bool) -> Vec<u8> {
        let mut p = vec![0u8; 4 + 20 + 16];
        let mut matrix = [0u8; 36];
        if rotated {
            // 90°:a=0, b=1, c=-1, d=0
            matrix[4..8].copy_from_slice(&0x0001_0000u32.to_be_bytes());
            matrix[12..16].copy_from_slice(&(-0x0001_0000i32).to_be_bytes());
        } else {
            matrix[0..4].copy_from_slice(&0x0001_0000u32.to_be_bytes());
            matrix[16..20].copy_from_slice(&0x0001_0000u32.to_be_bytes());
        }
        p.extend_from_slice(&matrix);
        p.extend_from_slice(&(w << 16).to_be_bytes());
        p.extend_from_slice(&(h << 16).to_be_bytes());
        bx(b"tkhd", &p)
    }

    /// ftyp + mdat + moov(音频轨 + 视频轨),mdat 在前以验证 seek 跳过
    pub fn mp4_file(w: u32, h: u32, rotated: bool) -> Vec<u8> {
        let audio_trak = bx(b"trak", &tkhd(0, 0, false)); // 音频轨宽高为 0,应被跳过
        let video_trak = bx(b"trak", &tkhd(w, h, rotated));
        let moov =
            bx(b"moov", &[mvhd(1000, 60_000), audio_trak, video_trak].concat());
        [bx(b"ftyp", b"isom0000"), bx(b"mdat", &[0u8; 500]), moov].concat()
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::mp4_file;
    use super::*;
    use std::io::Cursor;

    #[test]
    fn mp4_dimensions_and_duration() {
        let f = mp4_file(1920, 1080, false);
        let m = probe_video("video/mp4", &mut Cursor::new(f)).unwrap();
        assert_eq!((m.width, m.height, m.duration_secs), (1920, 1080, Some(60)));
    }

    #[test]
    fn mp4_rotation_swaps_dimensions() {
        // 手机竖拍:编码 1920x1080 + 90° 旋转矩阵 → 显示 1080x1920
        let f = mp4_file(1920, 1080, true);
        let m = probe_video("video/quicktime", &mut Cursor::new(f)).unwrap();
        assert_eq!((m.width, m.height), (1080, 1920));
    }

    #[test]
    fn mp4_garbage_returns_none() {
        assert!(probe_video("video/mp4", &mut Cursor::new(vec![0u8; 64])).is_none());
        assert!(probe_video("video/mp4", &mut Cursor::new(b"not a video".to_vec())).is_none());
    }

    #[test]
    fn unknown_mime_returns_none() {
        let f = mp4_file(1920, 1080, false);
        // mkv/webm 不再按视频消息发送,对应 MIME 一律不解析
        assert!(probe_video("video/x-matroska", &mut Cursor::new(f.clone())).is_none());
        assert!(probe_video("video/webm", &mut Cursor::new(f)).is_none());
    }
}
