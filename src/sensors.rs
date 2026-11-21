// CPU temperature / fan speed via LibreHardwareMonitor.
//
// Windows exposes no CPU temperature or fan RPM without a kernel driver. LibreHardwareMonitor ships one;
// with its "Remote Web Server" enabled it serves every sensor as JSON on localhost. A background thread
// polls that every couple of seconds so the UI thread never blocks. If LibreHardwareMonitor is not
// running the values simply read as "unavailable".
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::Mutex;
use std::time::Duration;

pub const NO_TEMP: i32 = i32::MIN;
pub static TEMP_TENTHS: AtomicI32 = AtomicI32::new(NO_TEMP); // tenths of a degree C
pub static FAN_RPM: AtomicI32 = AtomicI32::new(-1);
pub static CONTROL_TENTHS: AtomicI32 = AtomicI32::new(NO_TEMP); // duty cycle of the chosen fan's control, tenths of %
static CONTROL_ID: Mutex<String> = Mutex::new(String::new());
pub static LHM_UP: AtomicBool = AtomicBool::new(false);

pub fn control_available() -> bool {
    CONTROL_ID.lock().map(|g| !g.is_empty()).unwrap_or(false)
}

struct Node {
    text: String,
    value: String,
    image: String,
    sensor_id: String,
    kind: String,
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
        let mut n = Node { text: String::new(), value: String::new(), image: String::new(), sensor_id: String::new(), kind: String::new(), children: Vec::new() };
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
                "SensorId" => n.sensor_id = self.string().unwrap_or_default(),
                "Type" => n.kind = self.string().unwrap_or_default(),
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

// optional override: ACTIVITYMONITOR_FAN_NAME="fan #1" picks the fan whose name contains that text
fn fan_pref() -> &'static str {
    static PREF: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    PREF.get_or_init(|| std::env::var("ACTIVITYMONITOR_FAN_NAME").unwrap_or_default().to_ascii_lowercase())
}

type Pick = Option<(u8, f32)>; // (rank, value) - lower rank wins

fn walk(n: &Node, under_cpu: bool, under_gpu: bool, temp: &mut Pick, fan: &mut Pick, fan_id: &mut String) {
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
                if name.contains("chipset") {
                    return;
                }
                let rank = if !fan_pref().is_empty() && name.contains(fan_pref()) {
                    0
                } else if name.contains("cpu") {
                    1
                } else {
                    2
                };
                let better = match *fan {
                    None => true,
                    Some((r, f)) => rank < r || (rank == r && v > f),
                };
                if better {
                    *fan = Some((rank, v));
                    *fan_id = n.sensor_id.clone();
                }
            }
        }
    }
    for c in &n.children {
        walk(c, cpu, gpu, temp, fan, fan_id);
    }
}

fn find_control(n: &Node, id: &str) -> Option<f32> {
    if n.kind == "Control" && n.sensor_id == id {
        return parse_num(&n.value);
    }
    n.children.iter().find_map(|c| find_control(c, id))
}

fn addr() -> std::net::SocketAddr {
    let port: u16 = std::env::var("ACTIVITYMONITOR_LHM_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8085);
    std::net::SocketAddr::from(([127, 0, 0, 1], port))
}

/// Set the CPU fan duty cycle (percent), or hand control back to the BIOS/automatic curve with a negative value.
pub fn set_control(v: i32) -> bool {
    use std::io::{Read, Write};
    let id = match CONTROL_ID.lock() {
        Ok(g) if !g.is_empty() => g.clone(),
        _ => return false,
    };
    let val = if v < 0 { "null".to_string() } else { v.clamp(0, 100).to_string() };
    let Ok(mut s) = std::net::TcpStream::connect_timeout(&addr(), Duration::from_millis(300)) else { return false };
    let _ = s.set_read_timeout(Some(Duration::from_millis(1500)));
    let req = format!("GET /Sensor?action=Set&id={}&value={} HTTP/1.0\r\nHost: localhost\r\n\r\n", id, val);
    if s.write_all(req.as_bytes()).is_err() {
        return false;
    }
    let mut head = [0u8; 16];
    matches!(s.read(&mut head), Ok(n) if n >= 12 && &head[9..12] == b"200")
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
    let addr = addr();
    loop {
        let mut ok = false;
        if let Some(body) = fetch(&addr) {
            let mut j = Json { b: &body, i: 0 };
            if let Some(root) = j.node() {
                let (mut t, mut f) = (None, None);
                let mut fan_id = String::new();
                walk(&root, false, false, &mut t, &mut f, &mut fan_id);
                // the matching fan control sensor shares the hardware path: .../fan/N -> .../control/N
                let ctl_id = if fan_id.contains("/fan/") { fan_id.replace("/fan/", "/control/") } else { String::new() };
                let ctl = if ctl_id.is_empty() { None } else { find_control(&root, &ctl_id) };
                CONTROL_TENTHS.store(ctl.map_or(NO_TEMP, |v| (v * 10.0).round() as i32), Ordering::Relaxed);
                if let Ok(mut g) = CONTROL_ID.lock() {
                    *g = if ctl.is_some() { ctl_id } else { String::new() };
                }
                TEMP_TENTHS.store(t.map_or(NO_TEMP, |(_, v)| (v * 10.0).round() as i32), Ordering::Relaxed);
                FAN_RPM.store(f.map_or(-1, |(_, v)| v.round() as i32), Ordering::Relaxed);
                ok = true;
            }
        }
        LHM_UP.store(ok, Ordering::Relaxed);
        if !ok {
            CONTROL_TENTHS.store(NO_TEMP, Ordering::Relaxed);
            if let Ok(mut g) = CONTROL_ID.lock() {
                g.clear();
            }
            TEMP_TENTHS.store(NO_TEMP, Ordering::Relaxed);
            FAN_RPM.store(-1, Ordering::Relaxed);
        }
        std::thread::sleep(Duration::from_secs(if ok { 2 } else { 5 }));
    }
}
