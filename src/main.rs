// ActivityMonitor - tiny Task-Manager-style meters (CPU, memory, disks, network, GPU)
// that sit in the taskbar, just left of the notification area.
//
// Design goals: minimal CPU / memory / disk overhead.
//  * one 1 Hz timer, nothing runs in between
//  * cheap kernel counters only (GetSystemTimes, GlobalMemoryStatusEx,
//    IOCTL_DISK_PERFORMANCE, GetIfTable2, NVML / PDH for the GPU)
//  * software-rendered into a ~250x48 ARGB bitmap, pushed with UpdateLayeredWindow
//  * no allocation in the steady state, working set trimmed periodically
#![windows_subsystem = "windows"]
#![allow(static_mut_refs, dead_code)]

mod fanpanel;
mod sensors;

use core::ffi::c_void;
use core::fmt::Write as _;
use core::mem::{size_of, transmute, zeroed};
use core::ptr::{null, null_mut};
use std::sync::atomic::Ordering;
use std::time::Instant;

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::NetworkManagement::IpHelper::*;
use windows_sys::Win32::Storage::FileSystem::*;
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::*;
use windows_sys::Win32::System::LibraryLoader::*;
use windows_sys::Win32::System::Performance::*;
use windows_sys::Win32::System::Registry::*;
use windows_sys::Win32::System::SystemInformation::*;
use windows_sys::Win32::System::Threading::*;
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::HiDpi::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

// ---------------------------------------------------------------- constants
const N: usize = 8;
const HIST: usize = 128;
const K_CPU: usize = 0;
const K_TMP: usize = 1;
const K_FAN: usize = 2;
const K_MEM: usize = 3;
const K_DC: usize = 4;
const K_DD: usize = 5;
const K_NET: usize = 6;
const K_GPU: usize = 7;

const LABELS: [&[u8]; N] = [b"CPU", b"TMP", b"FAN", b"MEM", b"C:", b"D:", b"NET", b"GPU"];
// Task-Manager-like accent colours.
const COLORS: [(u32, u32, u32); N] = [
    (36, 165, 255),  // CPU  blue
    (255, 92, 92),   // TMP  red
    (236, 214, 84),  // FAN  yellow
    (198, 76, 240),  // MEM  purple
    (72, 224, 92),   // C:   green
    (72, 224, 92),   // D:   green
    (255, 160, 32),  // NET  orange
    (0, 220, 230),    // GPU  blue
];

const CELL_W: i32 = 50; // all sizes in 96-dpi pixels
const CELL_GAP: i32 = 6;
const PAD: i32 = 4;
const MARGIN_Y: i32 = 6;
const GAP_TO_TRAY: i32 = 2;

const ID_TIMER: usize = 1;
const IDM_TASKMGR: usize = 1;
const IDM_STARTUP: usize = 2;
const IDM_EXIT: usize = 3;

// ---------------------------------------------------------------- small helpers
struct Ascii {
    b: [u8; 96],
    n: usize,
}
impl core::fmt::Write for Ascii {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        // Latin-1 output (GDI ANSI text / tooltip widening), so U+00B0 degree sign is one byte
        for ch in s.chars() {
            if self.n < self.b.len() {
                self.b[self.n] = if (ch as u32) < 256 { ch as u32 as u8 } else { b'?' };
                self.n += 1;
            }
        }
        Ok(())
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn human_rate(b: f64, out: &mut Ascii) {
    // compact, <= 5 chars: 0B 512B 12K 340K 1.2M 25M 1.1G
    let (v, u) = if b < 1024.0 {
        (b, 'B')
    } else if b < 1024.0 * 1024.0 {
        (b / 1024.0, 'K')
    } else if b < 1024.0 * 1024.0 * 1024.0 {
        (b / 1048576.0, 'M')
    } else {
        (b / 1073741824.0, 'G')
    };
    if v < 10.0 && u != 'B' {
        let _ = write!(out, "{:.1}{}", v, u);
    } else {
        let _ = write!(out, "{:.0}{}", v, u);
    }
}

fn long_rate(b: f64, out: &mut Ascii) {
    if b < 1024.0 {
        let _ = write!(out, "{:.0} B/s", b);
    } else if b < 1048576.0 {
        let _ = write!(out, "{:.1} KB/s", b / 1024.0);
    } else if b < 1073741824.0 {
        let _ = write!(out, "{:.1} MB/s", b / 1048576.0);
    } else {
        let _ = write!(out, "{:.2} GB/s", b / 1073741824.0);
    }
}

// ---------------------------------------------------------------- GPU backends
#[repr(C)]
struct NvmlUtil {
    gpu: u32,
    mem: u32,
}
#[repr(C)]
struct NvmlMem {
    total: u64,
    free: u64,
    used: u64,
}
struct Nvml {
    handles: [*mut c_void; 4],
    count: usize,
    get_util: unsafe extern "system" fn(*mut c_void, *mut NvmlUtil) -> i32,
    get_mem: Option<unsafe extern "system" fn(*mut c_void, *mut NvmlMem) -> i32>,
}

struct PdhGpu {
    q: PDH_HQUERY,
    c: PDH_HCOUNTER,
    opened: Instant,
    buf: Vec<u64>,
    last: f64,
    ok: bool,
}

enum Gpu {
    None,
    Nvml(Nvml),
    Pdh(PdhGpu),
}

fn nvml_open() -> Option<Nvml> {
    unsafe {
        let mut lib = LoadLibraryW(wide("nvml.dll").as_ptr());
        if lib.is_null() {
            lib = LoadLibraryW(wide("C:\\Program Files\\NVIDIA Corporation\\NVSMI\\nvml.dll").as_ptr());
        }
        if lib.is_null() {
            return None;
        }
        let g = |n: &[u8]| GetProcAddress(lib, n.as_ptr());
        let init = g(b"nvmlInit_v2\0")?;
        let count = g(b"nvmlDeviceGetCount_v2\0")?;
        let by_idx = g(b"nvmlDeviceGetHandleByIndex_v2\0")?;
        let util = g(b"nvmlDeviceGetUtilizationRates\0")?;
        let init: unsafe extern "system" fn() -> i32 = transmute(init);
        if init() != 0 {
            return None;
        }
        let count: unsafe extern "system" fn(*mut u32) -> i32 = transmute(count);
        let by_idx: unsafe extern "system" fn(u32, *mut *mut c_void) -> i32 = transmute(by_idx);
        let mut n = 0u32;
        if count(&mut n) != 0 || n == 0 {
            return None;
        }
        let mut handles = [null_mut(); 4];
        let mut got = 0;
        for i in 0..(n.min(4)) {
            let mut h = null_mut();
            if by_idx(i, &mut h) == 0 {
                handles[got] = h;
                got += 1;
            }
        }
        if got == 0 {
            return None;
        }
        Some(Nvml {
            handles,
            count: got,
            get_util: transmute(util),
            get_mem: g(b"nvmlDeviceGetMemoryInfo\0").map(|p| transmute(p)),
        })
    }
}

fn pdh_open_query(p: &mut PdhGpu) {
    unsafe {
        if !p.q.is_null() {
            PdhCloseQuery(p.q);
            p.q = null_mut();
            p.c = null_mut();
        }
        p.ok = false;
        let mut q: PDH_HQUERY = null_mut();
        if PdhOpenQueryW(null(), 0, &mut q) != 0 {
            return;
        }
        let mut c: PDH_HCOUNTER = null_mut();
        let path = wide("\\GPU Engine(*)\\Utilization Percentage");
        if PdhAddEnglishCounterW(q, path.as_ptr(), 0, &mut c) != 0 {
            PdhCloseQuery(q);
            return;
        }
        PdhCollectQueryData(q);
        p.q = q;
        p.c = c;
        p.ok = true;
        p.opened = Instant::now();
    }
}

fn pdh_new() -> Option<PdhGpu> {
    let mut p = PdhGpu { q: null_mut(), c: null_mut(), opened: Instant::now(), buf: Vec::new(), last: 0.0, ok: false };
    pdh_open_query(&mut p);
    if p.ok {
        Some(p)
    } else {
        None
    }
}

// Highest per-engine-type utilisation (like Task Manager's GPU column).
fn pdh_sample(p: &mut PdhGpu) -> f64 {
    unsafe {
        if !p.ok {
            return p.last;
        }
        // wildcard instances are expanded when the counter is added; refresh now and then
        if p.opened.elapsed().as_secs() >= 20 {
            pdh_open_query(p);
            return p.last;
        }
        if PdhCollectQueryData(p.q) != 0 {
            return p.last;
        }
        let mut size = 0u32;
        let mut count = 0u32;
        PdhGetFormattedCounterArrayW(p.c, PDH_FMT_DOUBLE | 0x8000, &mut size, &mut count, null_mut());
        if size == 0 {
            return p.last;
        }
        p.buf.resize((size as usize + 7) / 8, 0);
        let items = p.buf.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W;
        if PdhGetFormattedCounterArrayW(p.c, PDH_FMT_DOUBLE | 0x8000, &mut size, &mut count, items) != 0 {
            return p.last;
        }
        let mut keys = [0u64; 16];
        let mut sums = [0f64; 16];
        let mut nb = 0usize;
        for i in 0..count as usize {
            let it = &*items.add(i);
            if it.FmtValue.CStatus != 0 {
                continue;
            }
            let v = it.FmtValue.Anonymous.doubleValue;
            // find "engtype_" and hash the suffix
            let name = it.szName;
            let mut len = 0;
            while *name.add(len) != 0 {
                len += 1;
            }
            let s = core::slice::from_raw_parts(name, len);
            let pat: [u16; 8] = [b'e' as u16, b'n' as u16, b'g' as u16, b't' as u16, b'y' as u16, b'p' as u16, b'e' as u16, b'_' as u16];
            let mut pos = None;
            if len >= 8 {
                for j in 0..=(len - 8) {
                    if s[j..j + 8] == pat {
                        pos = Some(j + 8);
                        break;
                    }
                }
            }
            let Some(pos) = pos else { continue };
            let mut h = 1469598103934665603u64;
            for &ch in &s[pos..] {
                h = (h ^ ch as u64).wrapping_mul(1099511628211);
            }
            let mut slot = nb;
            for k in 0..nb {
                if keys[k] == h {
                    slot = k;
                    break;
                }
            }
            if slot == nb {
                if nb == keys.len() {
                    continue;
                }
                keys[nb] = h;
                nb += 1;
            }
            sums[slot] += v;
        }
        let mut best = 0f64;
        for k in 0..nb {
            if sums[k] > best {
                best = sums[k];
            }
        }
        p.last = best.min(100.0);
        p.last
    }
}

// ---------------------------------------------------------------- application state
struct Disk {
    letter: u8,
    h: HANDLE,
    prev_idle: u64,
    prev_q: u64,
    prev_r: u64,
    prev_w: u64,
    have_prev: bool,
    rd: f64,
    wr: f64,
    last_try: u32,
}

struct App {
    hinst: HINSTANCE,
    ctl: HWND,
    view: HWND,
    taskbar: HWND,
    tb_msg: u32,
    // layout
    scale: f32,
    w: i32,
    h: i32,
    x: i32,
    y: i32,
    // surfaces
    screen_dc: HDC,
    mdc: HDC,
    bmp: HBITMAP,
    bits: *mut u32,
    tdc: HDC,
    tbmp: HBITMAP,
    tbits: *mut u32,
    font_label: HFONT,
    font_value: HFONT,
    old_bmp: HGDIOBJ,
    old_tbmp: HGDIOBJ,
    dark: bool,
    // which meters are shown
    show: [bool; N],
    tmp_ok: bool,
    fan_ok: bool,
    tray_units: u32,
    order: [usize; N],
    n_show: usize,
    // data
    hist: [[f32; HIST]; N],
    head: usize,
    count: usize,
    cur: [f64; N],
    // cpu
    p_idle: u64,
    p_busy: u64,
    // memory
    mem_used: u64,
    mem_total: u64,
    // disks
    disks: [Disk; 2],
    // network
    net_rx: u64,
    net_tx: u64,
    net_have: bool,
    net_down: f64,
    net_up: f64,
    net_t: Instant,
    // gpu
    gpu: Gpu,
    gpu_mem: (u64, u64),
    tick_n: u32,
    // ui
    hover: i32,
    tip: HWND,
    tracking: bool,
}

static mut APP: *mut App = null_mut();
fn app() -> &'static mut App {
    unsafe { &mut *APP }
}

fn ft(t: &FILETIME) -> u64 {
    ((t.dwHighDateTime as u64) << 32) | t.dwLowDateTime as u64
}

// ---------------------------------------------------------------- sampling
fn sample_cpu(a: &mut App) {
    unsafe {
        let (mut i, mut k, mut u): (FILETIME, FILETIME, FILETIME) = zeroed();
        if GetSystemTimes(&mut i, &mut k, &mut u) == 0 {
            return;
        }
        let idle = ft(&i);
        let total = ft(&k) + ft(&u);
        let busy = total - idle;
        let (di, db) = (idle.wrapping_sub(a.p_idle), busy.wrapping_sub(a.p_busy));
        if a.p_idle != 0 && di + db > 0 {
            a.cur[K_CPU] = 100.0 * db as f64 / (di + db) as f64;
        }
        a.p_idle = idle;
        a.p_busy = busy;
    }
}

fn sample_mem(a: &mut App) {
    unsafe {
        let mut m: MEMORYSTATUSEX = zeroed();
        m.dwLength = size_of::<MEMORYSTATUSEX>() as u32;
        if GlobalMemoryStatusEx(&mut m) != 0 {
            a.cur[K_MEM] = m.dwMemoryLoad as f64;
            a.mem_total = m.ullTotalPhys;
            a.mem_used = m.ullTotalPhys - m.ullAvailPhys;
        }
    }
}

fn sample_disk(a: &mut App, idx: usize, dt: f64) {
    unsafe {
        let tick = a.tick_n;
        let d = &mut a.disks[idx];
        if d.h == INVALID_HANDLE_VALUE {
            // volume may appear later (USB, mount): retry every ~10 s
            if tick.wrapping_sub(d.last_try) < 10 && d.last_try != 0 {
                return;
            }
            d.last_try = tick.max(1);
            let path = [b'\\' as u16, b'\\' as u16, b'.' as u16, b'\\' as u16, d.letter as u16, b':' as u16, 0];
            let h = CreateFileW(path.as_ptr(), 0, FILE_SHARE_READ | FILE_SHARE_WRITE, null(), OPEN_EXISTING, 0, null_mut());
            if h == INVALID_HANDLE_VALUE {
                return;
            }
            d.h = h;
            d.have_prev = false;
        }
        let mut p: DISK_PERFORMANCE = zeroed();
        let mut ret = 0u32;
        let ok = DeviceIoControl(
            d.h,
            IOCTL_DISK_PERFORMANCE,
            null(),
            0,
            &mut p as *mut _ as *mut c_void,
            size_of::<DISK_PERFORMANCE>() as u32,
            &mut ret,
            null_mut(),
        );
        if ok == 0 {
            CloseHandle(d.h);
            d.h = INVALID_HANDLE_VALUE;
            d.last_try = tick.max(1);
            return;
        }
        let idle = p.IdleTime as u64;
        let q = p.QueryTime as u64;
        let (r, w) = (p.BytesRead as u64, p.BytesWritten as u64);
        if d.have_prev && q > d.prev_q {
            let dq = (q - d.prev_q) as f64;
            let di = idle.saturating_sub(d.prev_idle) as f64;
            let act = (100.0 * (1.0 - di / dq)).clamp(0.0, 100.0);
            a.cur[K_DC + idx] = act;
            if dt > 0.0 {
                d.rd = r.saturating_sub(d.prev_r) as f64 / dt;
                d.wr = w.saturating_sub(d.prev_w) as f64 / dt;
            }
        }
        d.prev_idle = idle;
        d.prev_q = q;
        d.prev_r = r;
        d.prev_w = w;
        d.have_prev = true;
    }
}

fn sample_net(a: &mut App, dt: f64) {
    unsafe {
        let mut t: *mut MIB_IF_TABLE2 = null_mut();
        if GetIfTable2(&mut t) != 0 || t.is_null() {
            return;
        }
        let (mut rx, mut tx) = (0u64, 0u64);
        let n = (*t).NumEntries as usize;
        let rows = (*t).Table.as_ptr();
        for i in 0..n {
            let r = &*rows.add(i);
            // physical, connected adapters only (avoids double counting virtual/loopback)
            if r.Type == 24 || r.OperStatus != 1 || (r.InterfaceAndOperStatusFlags._bitfield & 1) == 0 {
                continue;
            }
            rx += r.InOctets;
            tx += r.OutOctets;
        }
        FreeMibTable(t as *const c_void);
        if a.net_have && dt > 0.0 {
            a.net_down = rx.saturating_sub(a.net_rx) as f64 / dt;
            a.net_up = tx.saturating_sub(a.net_tx) as f64 / dt;
            a.cur[K_NET] = a.net_down + a.net_up;
        }
        a.net_rx = rx;
        a.net_tx = tx;
        a.net_have = true;
    }
}

fn sample_sensors(a: &mut App) {
    let t = sensors::TEMP_TENTHS.load(Ordering::Relaxed);
    let f = sensors::FAN_RPM.load(Ordering::Relaxed);
    a.tmp_ok = t != sensors::NO_TEMP;
    a.fan_ok = f >= 0;
    a.cur[K_TMP] = if a.tmp_ok { t as f64 / 10.0 } else { 0.0 };
    a.cur[K_FAN] = if a.fan_ok { f as f64 } else { 0.0 };
}

fn sample_gpu(a: &mut App) {
    unsafe {
        match &mut a.gpu {
            Gpu::None => {}
            Gpu::Nvml(n) => {
                let mut best = 0u32;
                let (mut used, mut total) = (0u64, 0u64);
                for i in 0..n.count {
                    let mut u = NvmlUtil { gpu: 0, mem: 0 };
                    if (n.get_util)(n.handles[i], &mut u) == 0 && u.gpu > best {
                        best = u.gpu;
                    }
                    if let Some(gm) = n.get_mem {
                        let mut m = NvmlMem { total: 0, free: 0, used: 0 };
                        if gm(n.handles[i], &mut m) == 0 {
                            used += m.used;
                            total += m.total;
                        }
                    }
                }
                a.gpu_mem = (used, total);
                a.cur[K_GPU] = best as f64;
            }
            Gpu::Pdh(p) => a.cur[K_GPU] = pdh_sample(p),
        }
    }
}

fn tick(a: &mut App) {
    let now = Instant::now();
    let dt = now.duration_since(a.net_t).as_secs_f64();
    a.net_t = now;
    a.tick_n = a.tick_n.wrapping_add(1);

    sample_cpu(a);
    sample_mem(a);
    sample_disk(a, 0, dt);
    sample_disk(a, 1, dt);
    sample_net(a, dt);
    sample_gpu(a);
    sample_sensors(a);
    fanpanel::update(
        if a.fan_ok { a.cur[K_FAN] as i32 } else { -1 },
        if a.tmp_ok { a.cur[K_TMP] as f32 } else { -1.0 },
        sensors::LHM_UP.load(Ordering::Relaxed),
    );

    // visibility: D: only when the volume exists
    let show_d = a.disks[1].h != INVALID_HANDLE_VALUE;
    if a.show[K_DD] != show_d {
        a.show[K_DD] = show_d;
        rebuild_order(a);
        a.w = 0; // force re-layout
    }

    if a.tick_n == 1 {
        // steady metrics: start with a flat history instead of an empty graph
        sample_mem(a);
        a.hist[K_MEM] = [a.cur[K_MEM] as f32; HIST];
        a.count = HIST;
    }
    a.head = (a.head + 1) % HIST;
    for i in 0..N {
        a.hist[i][a.head] = a.cur[i] as f32;
    }
    if a.count < HIST {
        a.count += 1;
    }
}

fn rebuild_order(a: &mut App) {
    let mut n = 0;
    for i in 0..N {
        if a.show[i] {
            a.order[n] = i;
            n += 1;
        }
    }
    a.n_show = n;
}

// ---------------------------------------------------------------- pixel rendering
#[inline]
fn blend(px: &mut u32, r: u32, g: u32, b: u32, a: u32) {
    if a == 0 {
        return;
    }
    let d = *px;
    let (da, dr, dg, db) = (d >> 24, (d >> 16) & 255, (d >> 8) & 255, d & 255);
    let ia = 255 - a;
    let oa = a + (da * ia + 127) / 255;
    let or = (r * a + 127) / 255 + (dr * ia + 127) / 255;
    let og = (g * a + 127) / 255 + (dg * ia + 127) / 255;
    let ob = (b * a + 127) / 255 + (db * ia + 127) / 255;
    *px = (oa.min(255) << 24) | (or.min(255) << 16) | (og.min(255) << 8) | ob.min(255);
}

// signed distance to a rounded rectangle (negative inside)
#[inline]
fn rr_sdf(px: f32, py: f32, w: f32, h: f32, r: f32) -> f32 {
    let (cx, cy) = (w * 0.5, h * 0.5);
    let (qx, qy) = ((px - cx).abs() - (cx - r), (py - cy).abs() - (cy - r));
    let (ox, oy) = (qx.max(0.0), qy.max(0.0));
    (ox * ox + oy * oy).sqrt() + qx.max(qy).min(0.0) - r
}


// ---------------------------------------------------------------- tiny vector icons (14x14 unit grid)
// prim: (op, a, b, c, d): op 0 = add rect(x0,y0,x1,y1), 1 = cut rect, 2 = add circle(cx,cy,r,_), 3 = cut circle
type Prim = (u8, f32, f32, f32, f32);
const ICON_CPU: &[Prim] = &[
    (0, 3.0, 3.0, 11.0, 11.0), (1, 4.3, 4.3, 9.7, 9.7), (0, 5.6, 5.6, 8.4, 8.4),
    (0, 6.4, 0.8, 7.6, 3.0), (0, 6.4, 11.0, 7.6, 13.2), (0, 0.8, 6.4, 3.0, 7.6), (0, 11.0, 6.4, 13.2, 7.6),
];
const ICON_MEM: &[Prim] = &[
    (0, 1.0, 3.5, 13.0, 10.5), (1, 2.3, 4.8, 11.7, 9.2),
    (0, 3.5, 6.0, 5.5, 8.0), (0, 6.0, 6.0, 8.0, 8.0), (0, 8.5, 6.0, 10.5, 8.0),
];
const ICON_NVME: &[Prim] = &[
    (0, 1.0, 4.0, 13.0, 10.0), (1, 2.3, 5.3, 11.7, 8.7),
    (0, 3.5, 6.0, 7.0, 8.0), (2, 10.0, 7.0, 0.9, 0.0),
];
const ICON_HDD: &[Prim] = &[
    (0, 1.5, 1.5, 12.5, 12.5), (1, 2.8, 2.8, 11.2, 11.2),
    (2, 7.0, 7.0, 3.2, 0.0), (3, 7.0, 7.0, 2.0, 0.0), (2, 7.0, 7.0, 0.9, 0.0),
];
const ICON_NET: &[Prim] = &[
    (2, 7.0, 7.0, 5.8, 0.0), (3, 7.0, 7.0, 4.6, 0.0),
    (0, 6.4, 1.2, 7.6, 12.8), (0, 1.2, 6.4, 12.8, 7.6),
    (3, 7.0, 7.0, 0.0, 0.0), (2, 7.0, 7.0, 2.4, 0.0), (3, 7.0, 7.0, 1.2, 0.0),
];
const ICON_GPU: &[Prim] = &[
    (0, 1.0, 3.0, 13.0, 11.0), (1, 2.3, 4.3, 11.7, 9.7),
    (2, 7.0, 7.0, 2.6, 0.0), (3, 7.0, 7.0, 1.5, 0.0), (2, 7.0, 7.0, 0.7, 0.0),
    (0, 2.5, 11.0, 5.0, 12.5),
];
fn icon_for(m: usize) -> &'static [Prim] {
    match m {
        K_CPU => ICON_CPU,
        K_MEM => ICON_MEM,
        K_DC => ICON_NVME,
        K_DD => ICON_HDD,
        K_NET => ICON_NET,
        _ => ICON_GPU,
    }
}
fn icon_hit(prims: &[Prim], x: f32, y: f32) -> bool {
    let mut inside = false;
    for &(op, a, b, c, d) in prims {
        let hit = if op < 2 { x >= a && x < c && y >= b && y < d } else { (x - a) * (x - a) + (y - b) * (y - b) <= c * c };
        if hit {
            inside = op == 0 || op == 2;
        }
    }
    inside
}
fn draw_icon(px: &mut [u32], w: usize, h: usize, ox: f32, oy: f32, s: f32, prims: &[Prim], col: (u32, u32, u32), alpha: f32) {
    let size = (14.0 * s).ceil() as i32;
    for dy in 0..size {
        for dx in 0..size {
            let (x, y) = ((ox as i32 + dx) as usize, (oy as i32 + dy) as usize);
            if x >= w || y >= h {
                continue;
            }
            let mut n = 0;
            for sy in 0..3 {
                for sx in 0..3 {
                    let u = (dx as f32 + (sx as f32 + 0.5) / 3.0) / s;
                    let v = (dy as f32 + (sy as f32 + 0.5) / 3.0) / s;
                    if icon_hit(prims, u, v) {
                        n += 1;
                    }
                }
            }
            if n > 0 {
                blend(&mut px[y * w + x], col.0, col.1, col.2, (alpha * n as f32 / 9.0) as u32);
            }
        }
    }
}

fn create_surfaces(a: &mut App) {
    unsafe {
        free_surfaces(a);
        let mk = |a: &mut App| -> (HDC, HBITMAP, *mut u32) {
            let mut bi: BITMAPINFO = zeroed();
            bi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
            bi.bmiHeader.biWidth = a.w;
            bi.bmiHeader.biHeight = -a.h;
            bi.bmiHeader.biPlanes = 1;
            bi.bmiHeader.biBitCount = 32;
            bi.bmiHeader.biCompression = BI_RGB;
            let mut bits: *mut c_void = null_mut();
            let dc = CreateCompatibleDC(a.screen_dc);
            let bmp = CreateDIBSection(dc, &bi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
            (dc, bmp, bits as *mut u32)
        };
        let (d1, b1, p1) = mk(a);
        a.mdc = d1;
        a.bmp = b1;
        a.bits = p1;
        a.old_bmp = SelectObject(d1, b1);
        let (d2, b2, p2) = mk(a);
        a.tdc = d2;
        a.tbmp = b2;
        a.tbits = p2;
        a.old_tbmp = SelectObject(d2, b2);

        let s = a.scale;
        let face = wide("Segoe UI");
        let mkfont = |px: f32, weight: i32| {
            CreateFontW(
                -((px * s).round() as i32),
                0,
                0,
                0,
                weight,
                0,
                0,
                0,
                DEFAULT_CHARSET as u32,
                OUT_DEFAULT_PRECIS as u32,
                CLIP_DEFAULT_PRECIS as u32,
                CLEARTYPE_QUALITY as u32,
                (DEFAULT_PITCH | FF_DONTCARE) as u32,
                face.as_ptr(),
            )
        };
        a.font_label = mkfont(12.0, 400);
        a.font_value = mkfont(15.0, 400);
        SelectObject(a.tdc, a.font_label);
        SetBkMode(a.tdc, TRANSPARENT as i32);
        SetTextColor(a.tdc, 0x00FFFFFF);
    }
}

fn free_surfaces(a: &mut App) {
    unsafe {
        if !a.mdc.is_null() {
            SelectObject(a.mdc, a.old_bmp);
            DeleteObject(a.bmp);
            DeleteDC(a.mdc);
            a.mdc = null_mut();
            a.bmp = null_mut();
        }
        if !a.tdc.is_null() {
            SelectObject(a.tdc, a.old_tbmp);
            DeleteObject(a.tbmp);
            DeleteDC(a.tdc);
            a.tdc = null_mut();
            a.tbmp = null_mut();
        }
        if !a.font_label.is_null() {
            DeleteObject(a.font_label);
            DeleteObject(a.font_value);
            a.font_label = null_mut();
            a.font_value = null_mut();
        }
    }
}

fn cell_geometry(a: &App) -> (f32, f32, f32, f32, f32) {
    // returns (x_of_first, cell_w, cell_gap, y0, cell_h) in device pixels
    let s = a.scale;
    let cw = (CELL_W as f32 * s).round();
    let gap = (CELL_GAP as f32 * s).round();
    let y0 = (MARGIN_Y as f32 * s).round();
    let ch = a.h as f32 - 2.0 * y0;
    ((PAD as f32 * s).round(), cw, gap, y0, ch)
}

fn render(a: &mut App) {
    unsafe {
        if a.bits.is_null() || a.n_show == 0 {
            return;
        }
        let (w, h) = (a.w as usize, a.h as usize);
        let px = core::slice::from_raw_parts_mut(a.bits, w * h);
        px.fill(0);

        let s = a.scale;
        let (x_first, cw, gap, y0, ch) = cell_geometry(a);
        let rad = 7.0 * s;
        let bw = (1.0 * s).max(1.0);
        let lw = (1.6 * s).max(1.0);
        let dark = a.dark;
        let (_bg, bg_a): ((u32, u32, u32), u32) = if dark { ((0, 0, 0), 255) } else { ((0, 0, 0), 255) };

        // clear text mask
        core::slice::from_raw_parts_mut(a.tbits, w * h).fill(0);

        for slot in 0..a.n_show {
            let m = a.order[slot];
            let (cr, cg, cb) = COLORS[m];
            let cx0 = x_first + slot as f32 * (cw + gap);
            let x_start = cx0.floor() as usize;
            let x_end = ((cx0 + cw).ceil() as usize).min(w);
            let y_start = y0 as usize;
            let y_end = ((y0 + ch) as usize).min(h);
            let inner_w = (cw - 2.0 * bw) as usize;
            let ncols = inner_w.min(HIST).max(1);

            // graph scale
            let mut scale_max = 100.0f32;
            if m == K_NET || m == K_FAN {
                scale_max = if m == K_NET { 100.0 * 1024.0 } else { 2000.0 }; // floor, then auto-scale
                for k in 0..ncols.min(a.count) {
                    let v = a.hist[m][(a.head + HIST - k) % HIST];
                    if v > scale_max {
                        scale_max = v;
                    }
                }
                scale_max *= 1.1;
            }

            let inner_h = ch - 2.0 * bw;
            // smoothed column heights (px), newest at the right edge
            let mut hp = [0f32; 256];
            let nc = inner_w.min(256);
            for c in 0..nc {
                let k = nc - 1 - c;
                let mut acc = 0.0;
                let mut cnt = 0.0;
                for d in -2i32..=2 {
                    let kk = k as i32 + d;
                    if kk >= 0 && (kk as usize) < ncols && (kk as usize) < a.count {
                        acc += a.hist[m][(a.head + HIST - kk as usize) % HIST];
                        cnt += 1.0;
                    }
                }
                let v = if cnt > 0.0 { acc / cnt / scale_max } else { 0.0 };
                hp[c] = v.clamp(0.0, 1.0) * inner_h;
            }
            let (bgt, bgb): ((f32, f32, f32), (f32, f32, f32)) =
                if dark { ((40.0, 40.0, 46.0), (20.0, 20.0, 24.0)) } else { ((255.0, 255.0, 255.0), (236.0, 239.0, 244.0)) };
            for y in y_start..y_end {
                let fy = (y - y_start) as f32 + 0.5;
                let from_bottom = (y_end - 1 - y) as f32 + 0.5;
                let t = fy / ch;
                let (br, bgc, bb) = (bgt.0 + (bgb.0 - bgt.0) * t, bgt.1 + (bgb.1 - bgt.1) * t, bgt.2 + (bgb.2 - bgt.2) * t);
                for x in x_start..x_end {
                    let fx = x as f32 + 0.5 - cx0;
                    let sdf = rr_sdf(fx, fy, cw, ch, rad);
                    let cov = (0.5 - sdf).clamp(0.0, 1.0);
                    if cov <= 0.0 {
                        continue;
                    }
                    let p = &mut px[y * w + x];
                    blend(p, br as u32, bgc as u32, bb as u32, (bg_a as f32 * cov) as u32);

                    let col = (fx - bw) as i32;
                    if col >= 0 && (col as usize) < nc {
                        let c = col as usize;
                        let h0 = hp[c];
                        if h0 > 0.0 || from_bottom < 1.0 {
                            let slope = (hp[(c + 1).min(nc - 1)] - hp[c.saturating_sub(1)]).abs() * 0.5;
                            let below = (h0 - from_bottom + 0.5).clamp(0.0, 1.0);
                            if below > 0.0 {
                                // gradient: strongest just under the line, fading toward the bottom
                                let depth = ((h0 - from_bottom) / (h0.max(inner_h * 0.35))).clamp(0.0, 1.0);
                                blend(p, cr, cg, cb, ((30.0 + 105.0 * (1.0 - depth)) * below * cov) as u32);
                            }
                            let half = (lw + slope) * 0.5;
                            let line = (half + 0.5 - (from_bottom - h0).abs()).clamp(0.0, 1.0);
                            if line > 0.0 && h0 > 0.0 {
                                blend(p, cr, cg, cb, (255.0 * line * cov) as u32);
                            }
                        }
                    }
                    let bcov = cov - (0.5 - (sdf + bw)).clamp(0.0, 1.0);
                    if bcov > 0.0 {
                        blend(p, cr, cg, cb, (185.0 * bcov) as u32);
                    }
                }
            }

            // small label, top-left, drawn into the mask and tinted below
            let mut r1 = RECT { left: (cx0 + 6.0 * s) as i32, top: (y0 + 3.0 * s) as i32, right: (cx0 + cw) as i32, bottom: (y0 + 17.0 * s) as i32 };
            SelectObject(a.tdc, a.font_label);
            DrawTextA(a.tdc, LABELS[m].as_ptr(), LABELS[m].len() as i32, &mut r1, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
            if m == K_TMP || m == K_FAN {
                // these two have no other readout, so show the number too
                let ok = if m == K_TMP { a.tmp_ok } else { a.fan_ok };
                let mut val = Ascii { b: [0; 96], n: 0 };
                if !ok {
                    let _ = write!(val, "--");
                } else if m == K_TMP {
                    let _ = write!(val, "{:.0}\u{b0}", a.cur[K_TMP]);
                } else {
                    let _ = write!(val, "{:.0}", a.cur[K_FAN]);
                }
                let mut r2 = RECT { left: (cx0 + 6.0 * s) as i32, top: (y0 + 17.0 * s) as i32, right: (cx0 + cw) as i32, bottom: (y0 + ch - 1.0 * s) as i32 };
                SelectObject(a.tdc, a.font_value);
                DrawTextA(a.tdc, val.b.as_ptr(), val.n as i32, &mut r2, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
            }
        }

        let mask = core::slice::from_raw_parts(a.tbits, w * h);
        for y in 0..h {
            for x in 0..w {
                let mv = mask[y * w + x];
                if mv & 0x00FFFFFF == 0 {
                    continue;
                }
                let xf = x as f32 - x_first;
                let slot = ((xf / (cw + gap)) as usize).min(a.n_show - 1);
                let (cr, cg, cb) = COLORS[a.order[slot]];
                let c = if dark { ((cr + 255) / 2 + 25, (cg + 255) / 2 + 25, (cb + 255) / 2 + 25) } else { (cr / 2, cg / 2, cb / 2) };
                // ClearType: per-channel coverage (R,G,B) from the mask, applied to the opaque card colour
                let (cr_, cg_, cb_) = ((mv >> 16) & 255, (mv >> 8) & 255, mv & 255);
                let dst = &mut px[y * w + x];
                if (*dst >> 24) == 255 {
                    let mix = |d: u32, t: u32, k: u32| (t * k + d * (255 - k) + 127) / 255;
                    let (dr, dg, db) = ((*dst >> 16) & 255, (*dst >> 8) & 255, *dst & 255);
                    *dst = 0xFF00_0000 | (mix(dr, c.0.min(255), cr_) << 16) | (mix(dg, c.1.min(255), cg_) << 8) | mix(db, c.2.min(255), cb_);
                } else {
                    let cov = (cr_ + cg_ + cb_) / 3;
                    blend(dst, c.0.min(255), c.1.min(255), c.2.min(255), cov);
                }
            }
        }
    }
}
fn present(a: &mut App) {
    unsafe {
        if a.view.is_null() || a.mdc.is_null() {
            return;
        }
        let mut size = SIZE { cx: a.w, cy: a.h };
        let src = POINT { x: 0, y: 0 };
        let bf = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: 255, AlphaFormat: AC_SRC_ALPHA as u8 };
        UpdateLayeredWindow(a.view, a.screen_dc, null(), &mut size, a.mdc, &src, 0, &bf, ULW_ALPHA);
    }
}

// ---------------------------------------------------------------- window management / placement
const CLASS: &str = "ActivityMonitorWnd";

fn read_theme_dark() -> bool {
    unsafe {
        let key = wide("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize");
        let val = wide("SystemUsesLightTheme");
        let mut data = 0u32;
        let mut sz = 4u32;
        let r = RegGetValueW(HKEY_CURRENT_USER, key.as_ptr(), val.as_ptr(), RRF_RT_REG_DWORD, null_mut(), &mut data as *mut _ as *mut c_void, &mut sz);
        if r != 0 {
            return true;
        }
        data == 0
    }
}

const REG_KEY: &str = "Software\\ActivityMonitor";

fn load_tray_units() -> u32 {
    unsafe {
        let mut data = 0u32;
        let mut sz = 4u32;
        let r = RegGetValueW(HKEY_CURRENT_USER, wide(REG_KEY).as_ptr(), wide("TrayWidth").as_ptr(), RRF_RT_REG_DWORD, null_mut(), &mut data as *mut _ as *mut c_void, &mut sz);
        if r == 0 && data > 60 && data < 700 {
            data
        } else {
            0
        }
    }
}

fn save_tray_units(units: u32) {
    unsafe {
        let mut hk: HKEY = null_mut();
        if RegCreateKeyExW(HKEY_CURRENT_USER, wide(REG_KEY).as_ptr(), 0, null(), 0, KEY_SET_VALUE, null(), &mut hk, null_mut()) == 0 {
            RegSetValueExW(hk, wide("TrayWidth").as_ptr(), 0, REG_DWORD, &units as *const u32 as *const u8, 4);
            RegCloseKey(hk);
        }
    }
}

fn find_taskbar() -> HWND {
    unsafe { FindWindowW(wide("Shell_TrayWnd").as_ptr(), null()) }
}

fn ensure_view(a: &mut App) {
    unsafe {
        let tb = find_taskbar();
        if tb.is_null() {
            return;
        }
        if a.view.is_null() || a.taskbar != tb || IsWindow(a.view) == 0 {
            if !a.view.is_null() && IsWindow(a.view) != 0 {
                DestroyWindow(a.view);
            }
            a.taskbar = tb;
            let cls = wide(CLASS);
            // parent and child must share a DPI awareness context
            let old_ctx = SetThreadDpiAwarenessContext(GetWindowDpiAwarenessContext(tb));
            a.view = CreateWindowExW(
                WS_EX_LAYERED | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                cls.as_ptr(),
                null(),
                WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS,
                0,
                0,
                10,
                10,
                tb,
                null_mut(),
                a.hinst,
                null(),
            );
            SetThreadDpiAwarenessContext(old_ctx);
            a.w = 0;
            a.tip = null_mut();
            create_tooltip(a);
        }
    }
}

fn layout(a: &mut App) {
    unsafe {
        ensure_view(a);
        if a.view.is_null() {
            return;
        }
        let tb = a.taskbar;
        let dpi = GetDpiForWindow(tb).max(96);
        let scale = dpi as f32 / 96.0;
        let mut rtb: RECT = zeroed();
        GetWindowRect(tb, &mut rtb);
        let mut rtn: RECT = zeroed();
        let tray = FindWindowExW(tb, null_mut(), wide("TrayNotifyWnd").as_ptr(), null());
        if !tray.is_null() {
            GetWindowRect(tray, &mut rtn);
        }
        // Right after sign-in Explorer reports an empty TrayNotifyWnd rect for a while (and on some builds
        // forever). Only trust a plausible rect; otherwise fall back to the last width we saw.
        let tray_w = rtb.right - rtn.left;
        if !tray.is_null() && rtn.right > rtn.left && tray_w > (60.0 * scale) as i32 && tray_w < (700.0 * scale) as i32 {
            let units = (tray_w as f32 / scale).round() as u32;
            if units != a.tray_units {
                a.tray_units = units;
                save_tray_units(units);
            }
        } else {
            let units = if a.tray_units > 0 { a.tray_units } else { 240 };
            rtn.left = rtb.right - (units as f32 * scale) as i32;
        }
        a.scale = scale;
        let (x_first, cw, gap, _, _) = {
            let tmp_h = rtb.bottom - rtb.top;
            let old = a.h;
            a.h = tmp_h;
            let g = cell_geometry(a);
            a.h = old;
            g
        };
        let n = a.n_show as f32;
        let w = (x_first * 2.0 + n * cw + (n - 1.0).max(0.0) * gap) as i32;
        let h = rtb.bottom - rtb.top;
        let x = rtn.left - rtb.left - w - (GAP_TO_TRAY as f32 * scale) as i32;
        let y = 0;

        let size_changed = w != a.w || h != a.h;
        let moved = x != a.x || y != a.y;
        // keep our window the top-most child of the taskbar
        let not_top = GetWindow(tb, GW_CHILD) != a.view;
        if size_changed || moved || not_top {
            let flags = SWP_NOACTIVATE | SWP_SHOWWINDOW | if not_top || size_changed || moved { 0 } else { SWP_NOZORDER };
            SetWindowPos(a.view, HWND_TOP, x, y, w, h, flags);
            a.x = x;
            a.y = y;
        }
        if size_changed {
            a.w = w;
            a.h = h;
            create_surfaces(a);
            update_tooltip_rect(a);
            draw(a);
        }
    }
}

fn draw(a: &mut App) {
    render(a);
    present(a);
}

// ---------------------------------------------------------------- tooltip (native, shown on hover)
fn create_tooltip(a: &mut App) {
    unsafe {
        let icc = INITCOMMONCONTROLSEX { dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32, dwICC: ICC_WIN95_CLASSES };
        InitCommonControlsEx(&icc);
        let cls = wide("tooltips_class32");
        a.tip = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            cls.as_ptr(),
            null(),
            WS_POPUP | TTS_NOPREFIX | TTS_ALWAYSTIP,
            0,
            0,
            0,
            0,
            null_mut(),
            null_mut(),
            a.hinst,
            null(),
        );
        if a.tip.is_null() {
            return;
        }
        let mut ti: TTTOOLINFOW = zeroed();
        ti.cbSize = size_of::<TTTOOLINFOW>() as u32;
        ti.uFlags = TTF_TRACK | TTF_ABSOLUTE;
        ti.hwnd = a.view;
        ti.uId = 1;
        let mut init = [b'x' as u16, 0];
        ti.lpszText = init.as_mut_ptr(); // replaced on show
        SendMessageW(a.tip, TTM_ADDTOOLW, 0, &ti as *const _ as isize);
        SendMessageW(a.tip, TTM_SETMAXTIPWIDTH, 0, 400);
    }
}

fn update_tooltip_rect(_a: &mut App) {}

fn tip_text(a: &App, m: usize, out: &mut Ascii) {
    let gb = |b: u64| b as f64 / 1073741824.0;
    match m {
        K_CPU => {
            let _ = write!(out, "CPU  {:.0}%", a.cur[K_CPU]);
        }
        K_TMP => {
            if a.tmp_ok {
                let _ = write!(out, "CPU temperature  {:.1} \u{b0}C", a.cur[K_TMP]);
            } else {
                let _ = write!(out, "CPU temperature unavailable\nrun LibreHardwareMonitor with\nits Remote Web Server enabled");
            }
        }
        K_FAN => {
            if a.fan_ok {
                let _ = write!(out, "CPU fan  {:.0} RPM", a.cur[K_FAN]);
            } else {
                let _ = write!(out, "CPU fan unavailable\nrun LibreHardwareMonitor with\nits Remote Web Server enabled");
            }
        }
        K_MEM => {
            let _ = write!(out, "Memory  {:.0}%\n{:.1} / {:.1} GB in use", a.cur[K_MEM], gb(a.mem_used), gb(a.mem_total));
        }
        K_DC | K_DD => {
            let d = &a.disks[m - K_DC];
            let _ = write!(out, "Disk {}:  {:.0}% active\nRead ", d.letter as char, a.cur[m]);
            long_rate(d.rd, out);
            let _ = write!(out, "\nWrite ");
            long_rate(d.wr, out);
        }
        K_NET => {
            let _ = write!(out, "Network\nDown ");
            long_rate(a.net_down, out);
            let _ = write!(out, "\nUp   ");
            long_rate(a.net_up, out);
        }
        _ => {
            let _ = write!(out, "GPU  {:.0}%", a.cur[K_GPU]);
            if a.gpu_mem.1 > 0 {
                let _ = write!(out, "\nVRAM {:.1} / {:.1} GB", gb(a.gpu_mem.0), gb(a.gpu_mem.1));
            }
        }
    }
}

fn cell_at(a: &App, x: i32) -> i32 {
    let (x_first, cw, gap, _, _) = cell_geometry(a);
    let xf = x as f32 - x_first;
    if xf < 0.0 {
        return -1;
    }
    let slot = (xf / (cw + gap)) as usize;
    if slot < a.n_show && xf - slot as f32 * (cw + gap) <= cw {
        a.order[slot] as i32
    } else {
        -1
    }
}

fn cell_screen_rect(a: &App, m: usize) -> RECT {
    unsafe {
        let slot = (0..a.n_show).find(|&i| a.order[i] == m).unwrap_or(0);
        let (x_first, cw, gap, y0, ch) = cell_geometry(a);
        let mut r: RECT = zeroed();
        GetWindowRect(a.view, &mut r);
        let l = r.left + (x_first + slot as f32 * (cw + gap)) as i32;
        RECT { left: l, top: r.top + y0 as i32, right: l + cw as i32, bottom: r.top + (y0 + ch) as i32 }
    }
}

fn show_tip(a: &mut App) {
    unsafe {
        if a.tip.is_null() {
            return;
        }
        let mut ti: TTTOOLINFOW = zeroed();
        ti.cbSize = size_of::<TTTOOLINFOW>() as u32;
        ti.hwnd = a.view;
        ti.uId = 1;
        if a.hover < 0 {
            SendMessageW(a.tip, TTM_TRACKACTIVATE, 0, &ti as *const _ as isize);
            return;
        }
        if a.hover as usize == K_FAN {
            // the FAN meter shows the fan-control popup instead of a plain tooltip
            SendMessageW(a.tip, TTM_TRACKACTIVATE, 0, &ti as *const _ as isize);
            fanpanel::show(cell_screen_rect(a, K_FAN), a.scale, false);
            return;
        }
        let mut t = Ascii { b: [0; 96], n: 0 };
        tip_text(a, a.hover as usize, &mut t);
        let mut ws = [0u16; 100];
        for i in 0..t.n {
            ws[i] = t.b[i] as u16;
        }
        ti.lpszText = ws.as_mut_ptr();
        SendMessageW(a.tip, TTM_UPDATETIPTEXTW, 0, &ti as *const _ as isize);
        // position above the taskbar, centred over the cell
        let slot = (0..a.n_show).find(|&i| a.order[i] as i32 == a.hover).unwrap_or(0);
        let (x_first, cw, gap, _, _) = cell_geometry(a);
        let cx = a.x as f32 + x_first + slot as f32 * (cw + gap) + cw * 0.5;
        let mut r: RECT = zeroed();
        GetWindowRect(a.view, &mut r);
        let pos = ((cx as i32 + (r.left - a.x)) as isize & 0xFFFF) | (((r.top - 6) as isize & 0xFFFF) << 16);
        SendMessageW(a.tip, TTM_TRACKPOSITION, 0, pos);
        SendMessageW(a.tip, TTM_TRACKACTIVATE, 1, &ti as *const _ as isize);
        // shift so the tip sits fully above the cell
        let mut tr: RECT = zeroed();
        GetWindowRect(a.tip, &mut tr);
        SetWindowPos(a.tip, HWND_TOPMOST, cx as i32 + (r.left - a.x) - (tr.right - tr.left) / 2, r.top - (tr.bottom - tr.top) - 6, 0, 0, SWP_NOSIZE | SWP_NOACTIVATE);
    }
}

// ---------------------------------------------------------------- misc
fn startup_enabled() -> bool {
    unsafe {
        let key = wide("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
        let val = wide("ActivityMonitor");
        RegGetValueW(HKEY_CURRENT_USER, key.as_ptr(), val.as_ptr(), RRF_RT_REG_SZ, null_mut(), null_mut(), null_mut()) == 0
    }
}

fn set_startup(on: bool) {
    unsafe {
        let key = wide("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
        let mut hk: HKEY = null_mut();
        if RegOpenKeyExW(HKEY_CURRENT_USER, key.as_ptr(), 0, KEY_SET_VALUE, &mut hk) != 0 {
            return;
        }
        let val = wide("ActivityMonitor");
        if on {
            let mut path = [0u16; 520];
            let n = GetModuleFileNameW(null_mut(), path.as_mut_ptr().add(1), 512) as usize;
            path[0] = b'"' as u16;
            path[n + 1] = b'"' as u16;
            RegSetValueExW(hk, val.as_ptr(), 0, REG_SZ, path.as_ptr() as *const u8, ((n + 3) * 2) as u32);
        } else {
            RegDeleteValueW(hk, val.as_ptr());
        }
        RegCloseKey(hk);
    }
}

fn context_menu(a: &mut App) {
    unsafe {
        let m = CreatePopupMenu();
        AppendMenuW(m, MF_STRING, IDM_TASKMGR, wide("Open Task Manager").as_ptr());
        AppendMenuW(m, MF_STRING | if startup_enabled() { MF_CHECKED } else { 0 }, IDM_STARTUP, wide("Start with Windows").as_ptr());
        AppendMenuW(m, MF_SEPARATOR, 0, null());
        AppendMenuW(m, MF_STRING, IDM_EXIT, wide("Exit").as_ptr());
        let mut pt = POINT { x: 0, y: 0 };
        GetCursorPos(&mut pt);
        SetForegroundWindow(a.ctl);
        let cmd = TrackPopupMenu(m, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN, pt.x, pt.y, 0, a.ctl, null());
        DestroyMenu(m);
        PostMessageW(a.ctl, WM_NULL, 0, 0);
        match cmd as usize {
            IDM_TASKMGR => open_taskmgr(),
            IDM_STARTUP => set_startup(!startup_enabled()),
            IDM_EXIT => {
                PostMessageW(a.ctl, WM_CLOSE, 0, 0);
            }
            _ => {}
        }
    }
}

fn open_taskmgr() {
    unsafe {
        ShellExecuteW(null_mut(), wide("open").as_ptr(), wide("taskmgr.exe").as_ptr(), null(), null(), SW_SHOWNORMAL);
    }
}

fn trim_memory() {
    unsafe {
        SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX);
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let a = app();
    if hwnd == a.view {
        match msg {
            WM_MOUSEMOVE => {
                let x = (lp & 0xFFFF) as i16 as i32;
                if !a.tracking {
                    let mut te = TRACKMOUSEEVENT { cbSize: size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: hwnd, dwHoverTime: 0 };
                    TrackMouseEvent(&mut te);
                    a.tracking = true;
                }
                let c = cell_at(a, x);
                if c != a.hover {
                    a.hover = c;
                    show_tip(a);
                }
                return 0;
            }
            WM_MOUSELEAVE => {
                a.tracking = false;
                a.hover = -1;
                show_tip(a);
                return 0;
            }
            WM_RBUTTONUP => {
                a.hover = -1;
                show_tip(a);
                context_menu(a);
                return 0;
            }
            WM_LBUTTONDOWN => {
                if cell_at(a, (lp & 0xFFFF) as i16 as i32) == K_FAN as i32 {
                    fanpanel::show(cell_screen_rect(a, K_FAN), a.scale, true);
                }
                return 0;
            }
            WM_MOUSEWHEEL => {
                if a.hover == K_FAN as i32 {
                    fanpanel::wheel((((wp >> 16) & 0xFFFF) as i16 as i32) / 120 * 2);
                }
                return 0;
            }
            WM_LBUTTONDBLCLK => {
                open_taskmgr();
                return 0;
            }
            WM_MOUSEACTIVATE => return MA_NOACTIVATE as LRESULT,
            WM_NCHITTEST => return HTCLIENT as LRESULT,
            _ => {}
        }
        return DefWindowProcW(hwnd, msg, wp, lp);
    }

    match msg {
        WM_TIMER => {
            tick(a);
            layout(a);
            draw(a);
            if a.hover >= 0 {
                show_tip(a); // keep the tooltip text live
            }
            if a.tick_n % 60 == 1 {
                trim_memory();
            }
            0
        }
        WM_SETTINGCHANGE | WM_DISPLAYCHANGE | WM_DPICHANGED | WM_THEMECHANGED => {
            a.dark = read_theme_dark();
            a.w = 0;
            layout(a);
            draw(a);
            0
        }
        WM_CLOSE => {
            fanpanel::shutdown();
            DestroyWindow(hwnd);
            0
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            0
        }
        _ => {
            if msg == a.tb_msg && a.tb_msg != 0 {
                // explorer restarted: re-attach
                a.view = null_mut();
                a.taskbar = null_mut();
                layout(a);
                draw(a);
                return 0;
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }
    }
}

fn main() {
    unsafe {
        // single instance
        let mx = CreateMutexW(null(), 0, wide("Local\\ActivityMonitorSingleton").as_ptr());
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return;
        }
        let _ = mx;
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

        let hinst = GetModuleHandleW(null());
        let cls = wide(CLASS);
        let wc = WNDCLASSW {
            style: CS_DBLCLKS,
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: null_mut(),
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            hbrBackground: null_mut(),
            lpszMenuName: null(),
            lpszClassName: cls.as_ptr(),
        };
        RegisterClassW(&wc);

        let mut gpu = Gpu::None;
        let force_pdh = std::env::args().any(|x| x == "--pdh");
        if !force_pdh {
            if let Some(n) = nvml_open() {
                gpu = Gpu::Nvml(n);
            }
        }
        if let Gpu::None = gpu {
            if let Some(p) = pdh_new() {
                gpu = Gpu::Pdh(p);
            }
        }
        let has_gpu = !matches!(gpu, Gpu::None);

        std::thread::spawn(sensors::poll_forever);
        let mk_disk = |l: u8| Disk { letter: l, h: INVALID_HANDLE_VALUE, prev_idle: 0, prev_q: 0, prev_r: 0, prev_w: 0, have_prev: false, rd: 0.0, wr: 0.0, last_try: 0 };
        let app_box = Box::new(App {
            hinst,
            ctl: null_mut(),
            view: null_mut(),
            taskbar: null_mut(),
            tb_msg: RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()),
            scale: 1.0,
            w: 0,
            h: 0,
            x: i32::MIN,
            y: i32::MIN,
            screen_dc: GetDC(null_mut()),
            mdc: null_mut(),
            bmp: null_mut(),
            bits: null_mut(),
            tdc: null_mut(),
            tbmp: null_mut(),
            tbits: null_mut(),
            font_label: null_mut(),
            font_value: null_mut(),
            old_bmp: null_mut(),
            old_tbmp: null_mut(),
            dark: read_theme_dark(),
            show: [true, true, true, true, true, false, true, has_gpu],
            tmp_ok: false,
            fan_ok: false,
            tray_units: load_tray_units(),
            order: [0; N],
            n_show: 0,
            hist: [[0.0; HIST]; N],
            head: 0,
            count: 0,
            cur: [0.0; N],
            p_idle: 0,
            p_busy: 0,
            mem_used: 0,
            mem_total: 0,
            disks: [mk_disk(b'C'), mk_disk(b'D')],
            net_rx: 0,
            net_tx: 0,
            net_have: false,
            net_down: 0.0,
            net_up: 0.0,
            net_t: Instant::now(),
            gpu,
            gpu_mem: (0, 0),
            tick_n: 0,
            hover: -1,
            tip: null_mut(),
            tracking: false,
        });
        APP = Box::into_raw(app_box);
        let a = app();
        rebuild_order(a);
        fanpanel::init();

        a.ctl = CreateWindowExW(WS_EX_TOOLWINDOW, cls.as_ptr(), wide("ActivityMonitor").as_ptr(), WS_POPUP, 0, 0, 0, 0, null_mut(), null_mut(), hinst, null());

        // first sample establishes baselines; second (1 s later) gives real numbers
        tick(a);
        layout(a);
        draw(a);
        SetTimer(a.ctl, ID_TIMER, 1000, None);
        trim_memory();

        let mut msg: MSG = zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
