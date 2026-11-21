// CPU temperature / fan speed via LibreHardwareMonitor.
//
// Windows exposes no CPU temperature or fan RPM without a kernel driver. LibreHardwareMonitor ships one;
// with its "Remote Web Server" enabled it serves every sensor as JSON on localhost. A background thread
// polls that every couple of seconds so the UI thread never blocks. If LibreHardwareMonitor is not
// running the values simply read as "unavailable".
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;

pub const NO_TEMP: i32 = i32::MIN;
pub static TEMP_TENTHS: AtomicI32 = AtomicI32::new(NO_TEMP); // tenths of a degree C
pub static FAN_RPM: AtomicI32 = AtomicI32::new(-1);

struct Node {
    text: String,
    value: String,
    image: String,
    children: Vec<Node>,
}

struct Json<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Json<'a> {
    fn ws(&mut self) {
        while self.i < self.b.len() && self.b[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
    }

    fn eat(&mut self, c: u8) -> bool {
        self.ws();
        if self.i < self.b.len() && self.b[self.i] == c {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn string(&mut self) -> Option<String> {
        self.ws();
        if self.b.get(self.i) != Some(&b'"') {
            return None;
        }
        self.i += 1;
        let mut out: Vec<u8> = Vec::new();
        while self.i < self.b.len() {
            let c = self.b[self.i];
            self.i += 1;
            match c {
                b'"' => return Some(String::from_utf8_lossy(&out).into_owned()),
                b'\\' => {
                    let e = *self.b.get(self.i)?;
                    self.i += 1;
                    match e {
                        b'n' => out.push(b'\n'),
                        b't' => out.push(b'\t'),
                        b'u' => {
                            let h = core::str::from_utf8(self.b.get(self.i..self.i + 4)?).ok()?;
                            self.i += 4;
                            let ch = char::from_u32(u32::from_str_radix(h, 16).ok()?).unwrap_or('?');
                            let mut buf = [0u8; 4];
                            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                        }
                        o => out.push(o),
                    }
                }
                c => out.push(c),
            }
        }
        None
    }

    fn skip(&mut self) -> Option<()> {
        self.ws();
        match *self.b.get(self.i)? {
            b'"' => {
                self.string()?;
            }
            b'{' | b'[' => {
                let mut depth = 0;
                while self.i < self.b.len() {
                    match self.b[self.i] {
                        b'"' => {
                            self.string()?;
                            continue;
                        }
                        b'{' | b'[' => depth += 1,
                        b'}' | b']' => {
                            depth -= 1;
                            if depth == 0 {
                                self.i += 1;
                                return Some(());
                            }
                        }
                        _ => {}
                    }
                    self.i += 1;
                }
                return None;
            }
            _ => {
                while self.i < self.b.len() && !matches!(self.b[self.i], b',' | b'}' | b']') {
                    self.i += 1;
                }
            }
        }
        Some(())
    }

    fn node(&mut self) -> Option<Node> {
        if !self.eat(b'{') {
            return None;
        }
        let mut n = Node { text: String::new(), value: String::new(), image: String::new(), children: Vec::new() };
        loop {
            if self.eat(b'}') {
                return Some(n);
            }
            self.eat(b',');
            let key = self.string()?;
            if !self.eat(b':') {
                return None;
            }
            match key.as_str() {
                "Text" => n.text = self.string()?,
                "Value" => n.value = self.string().unwrap_or_default(),
                "ImageURL" => n.image = self.string().unwrap_or_default(),
                "Children" => {
                    if !self.eat(b'[') {
                        return None;
                    }
                    loop {
                        if self.eat(b']') {
                            break;
                        }
                        self.eat(b',');
                        n.children.push(self.node()?);
                    }
                }
                _ => self.skip()?,
            }
        }
    }
}

// "45,5 °C" / "1 203 RPM" -> number (decimal comma tolerated)
fn parse_num(v: &str) -> Option<f32> {
    let mut t = String::new();
    for c in v.chars() {
        if c.is_ascii_digit() || c == '-' {
            t.push(c);
        } else if c == '.' || c == ',' {
            t.push('.');
        } else if c == ' ' && t.is_empty() {
            continue;
        } else {
            break;
        }
    }
    t.parse().ok()
}

type Pick = Option<(u8, f32)>; // (rank, value) - lower rank wins

fn walk(n: &Node, under_cpu: bool, under_gpu: bool, temp: &mut Pick, fan: &mut Pick) {
    let img = n.image.to_ascii_lowercase();
    let cpu = under_cpu || img.contains("cpu");
    let gpu = under_gpu || img.contains("gpu") || img.contains("nvidia") || img.contains("ati");
    if n.children.is_empty() {
        let name = n.text.to_ascii_lowercase();
        if let Some(v) = parse_num(&n.value) {
            if n.value.ends_with('C') && cpu {
                let rank = if name.contains("tctl") || name.contains("tdie") || name.contains("package") {
                    0
                } else if name.contains("max") {
                    1
                } else if name.contains("average") {
                    3
                } else {
                    2
                };
                if temp.map_or(true, |(r, _)| rank < r) {
                    *temp = Some((rank, v));
                }
            } else if n.value.ends_with("RPM") && !gpu {
                // prefer a fan named "CPU"; otherwise the fastest spinning one
                let rank = if name.contains("cpu") { 0 } else { 1 };
                let better = match *fan {
                    None => true,
                    Some((r, f)) => rank < r || (rank == r && v > f),
                };
                if better {
                    *fan = Some((rank, v));
                }
            }
        }
    }
    for c in &n.children {
        walk(c, cpu, gpu, temp, fan);
    }
}

fn fetch(addr: &std::net::SocketAddr) -> Option<Vec<u8>> {
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect_timeout(addr, Duration::from_millis(200)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    s.write_all(b"GET /data.json HTTP/1.0\r\nHost: localhost\r\n\r\n").ok()?;
    let mut buf = Vec::with_capacity(64 * 1024);
    s.take(8 * 1024 * 1024).read_to_end(&mut buf).ok()?;
    let pos = buf.windows(4).position(|w| w == b"\r\n\r\n")?;
    buf.drain(..pos + 4);
    Some(buf)
}

pub fn poll_forever() {
    let port: u16 = std::env::var("ACTIVITYMONITOR_LHM_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8085);
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    loop {
        let mut ok = false;
        if let Some(body) = fetch(&addr) {
            let mut j = Json { b: &body, i: 0 };
            if let Some(root) = j.node() {
                let (mut t, mut f) = (None, None);
                walk(&root, false, false, &mut t, &mut f);
                TEMP_TENTHS.store(t.map_or(NO_TEMP, |(_, v)| (v * 10.0).round() as i32), Ordering::Relaxed);
                FAN_RPM.store(f.map_or(-1, |(_, v)| v.round() as i32), Ordering::Relaxed);
                ok = true;
            }
        }
        if !ok {
            TEMP_TENTHS.store(NO_TEMP, Ordering::Relaxed);
            FAN_RPM.store(-1, Ordering::Relaxed);
        }
        std::thread::sleep(Duration::from_secs(if ok { 2 } else { 5 }));
    }
}
