use toyos_abi::syscall;
use window::{Color, Framebuffer, Window};

const BG: Color = Color { r: 0x1e, g: 0x1e, b: 0x2e };
const FG: Color = Color { r: 0xcd, g: 0xd6, b: 0xf4 };
const HEADER_COLOR: Color = Color { r: 0x89, g: 0xb4, b: 0xfa };
const GREEN: Color = Color { r: 0xa6, g: 0xe3, b: 0xa1 };
const YELLOW: Color = Color { r: 0xf9, g: 0xe2, b: 0xaf };
const RED: Color = Color { r: 0xf3, g: 0x8b, b: 0xa8 };
const DIM: Color = Color { r: 0x58, g: 0x5b, b: 0x70 };
const BAR_BG: Color = Color { r: 0x31, g: 0x32, b: 0x44 };

const HEADER_SIZE: usize = 48;
const ENTRY_SIZE: usize = 48;

struct SysInfo {
    total_memory: u64,
    used_memory: u64,
    cpu_count: u32,
    process_count: u32,
    uptime_nanos: u64,
    cpu_busy_ticks: u64,
    cpu_total_ticks: u64,
    processes: Vec<ProcessInfo>,
}

struct ProcessInfo {
    pid: u32,
    parent_pid: u32,
    state: u8,
    is_thread: bool,
    memory_bytes: u64,
    name: String,
}

fn query_sysinfo() -> SysInfo {
    let mut buf = vec![0u8; 8192];
    let n = syscall::sysinfo(&mut buf);

    let total_memory = u64::from_le_bytes(buf[0..8].try_into().unwrap());
    let used_memory = u64::from_le_bytes(buf[8..16].try_into().unwrap());
    let cpu_count = u32::from_le_bytes(buf[16..20].try_into().unwrap());
    let process_count = u32::from_le_bytes(buf[20..24].try_into().unwrap());
    let uptime_nanos = u64::from_le_bytes(buf[24..32].try_into().unwrap());
    let cpu_busy_ticks = u64::from_le_bytes(buf[32..40].try_into().unwrap());
    let cpu_total_ticks = u64::from_le_bytes(buf[40..48].try_into().unwrap());

    let mut processes = Vec::new();
    let mut pos = HEADER_SIZE;
    while pos + ENTRY_SIZE <= n {
        let pid = u32::from_le_bytes(buf[pos..pos + 4].try_into().unwrap());
        let parent_pid = u32::from_le_bytes(buf[pos + 4..pos + 8].try_into().unwrap());
        let state = buf[pos + 8];
        let is_thread = buf[pos + 9] != 0;
        let memory_bytes = u64::from_le_bytes(buf[pos + 12..pos + 20].try_into().unwrap());

        let name_bytes = &buf[pos + 20..pos + 48];
        let name_len = name_bytes.iter().position(|&b| b == 0).unwrap_or(28);
        let name = String::from_utf8_lossy(&name_bytes[..name_len]).into_owned();

        processes.push(ProcessInfo { pid, parent_pid, state, is_thread, memory_bytes, name });
        pos += ENTRY_SIZE;
    }

    SysInfo { total_memory, used_memory, cpu_count, process_count, uptime_nanos, cpu_busy_ticks, cpu_total_ticks, processes }
}

fn state_str(state: u8) -> &'static str {
    match state {
        0 => "Running",
        1 => "Ready",
        2 => "Blocked",
        3 => "Zombie",
        _ => "?",
    }
}

fn state_color(state: u8) -> Color {
    match state {
        0 => GREEN,
        1 => HEADER_COLOR,
        2 => YELLOW,
        3 => RED,
        _ => FG,
    }
}

fn format_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1}M", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{}K", bytes / 1024)
    } else {
        format!("{}B", bytes)
    }
}

fn format_uptime(nanos: u64) -> String {
    let secs = nanos / 1_000_000_000;
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    format!("{:02}:{:02}:{:02}", h, m, s)
}

fn render(fb: &Framebuffer, font: &font::Font, info: &SysInfo) {
    let fw = font.width();
    let fh = font.height();

    // Clear background
    fb.fill_rect(0, 0, fb.width(), fb.height(), BG);

    let mut y = fh;
    let x = fw * 2;

    // Title
    font.draw_string(fb, x, y, "ToyOS System Monitor", HEADER_COLOR, BG);
    y += fh * 2;

    // Memory
    let total_mb = info.total_memory / (1024 * 1024);
    let used_mb = info.used_memory / (1024 * 1024);
    let mem_label = format!("Memory    {} MB / {} MB", used_mb, total_mb);
    font.draw_string(fb, x, y, &mem_label, FG, BG);

    // Memory bar
    let bar_x = x + fw * (mem_label.len() + 2);
    let bar_w = 20;
    let filled = if info.total_memory > 0 {
        (bar_w as u64 * info.used_memory / info.total_memory) as usize
    } else {
        0
    };

    font.draw_char(fb, bar_x, y, '[', DIM, BG);
    for i in 0..bar_w {
        let ch = if i < filled { '|' } else { ' ' };
        let color = if i < filled { GREEN } else { BAR_BG };
        font.draw_char(fb, bar_x + fw * (i + 1), y, ch, color, if i < filled { BG } else { BAR_BG });
    }
    font.draw_char(fb, bar_x + fw * (bar_w + 1), y, ']', DIM, BG);
    y += fh;

    // Uptime
    font.draw_string(fb, x, y, &format!("Uptime    {}", format_uptime(info.uptime_nanos)), FG, BG);
    y += fh;

    // CPU usage
    let cpu_pct = if info.cpu_total_ticks > 0 {
        info.cpu_busy_ticks * 100 / info.cpu_total_ticks
    } else {
        0
    };
    font.draw_string(fb, x, y, &format!("CPU       {}% ({} cores)", cpu_pct, info.cpu_count), FG, BG);
    y += fh;

    // Process count
    font.draw_string(fb, x, y, &format!("Processes {}", info.process_count), FG, BG);
    y += fh * 2;

    // Process table header
    let hdr = format!("{:<6}{:<6}{:<10}{:<10}{}", "PID", "PPID", "STATE", "MEMORY", "NAME");
    font.draw_string(fb, x, y, &hdr, HEADER_COLOR, BG);
    y += fh;

    // Separator
    let sep_w = hdr.len().min((fb.width() - x * 2) / fw);
    for i in 0..sep_w {
        font.draw_char(fb, x + i * fw, y, '-', DIM, BG);
    }
    y += fh;

    // Process entries
    for proc in &info.processes {
        if y + fh > fb.height() - fh * 2 {
            break;
        }

        let ppid = if proc.parent_pid == u32::MAX {
            "-".to_string()
        } else {
            proc.parent_pid.to_string()
        };

        let prefix = if proc.is_thread { "  " } else { "" };
        let name_display = if proc.name.is_empty() {
            if proc.is_thread { "(thread)" } else { "?" }
        } else {
            &proc.name
        };

        let pid_str = format!("{:<6}", proc.pid);
        let ppid_str = format!("{:<6}", ppid);
        let state = state_str(proc.state);
        let state_str_padded = format!("{:<10}", state);
        let mem_str = format!("{:<10}", format_bytes(proc.memory_bytes));
        let name_str = format!("{}{}", prefix, name_display);

        // Draw each column with appropriate color
        let mut cx = x;
        font.draw_string(fb, cx, y, &pid_str, FG, BG);
        cx += fw * 6;
        font.draw_string(fb, cx, y, &ppid_str, DIM, BG);
        cx += fw * 6;
        font.draw_string(fb, cx, y, &state_str_padded, state_color(proc.state), BG);
        cx += fw * 10;
        font.draw_string(fb, cx, y, &mem_str, FG, BG);
        cx += fw * 10;
        font.draw_string(fb, cx, y, &name_str, FG, BG);

        y += fh;
    }

    // Footer
    let footer_y = fb.height() - fh * 2;
    font.draw_string(fb, x, footer_y, "Press any key to refresh", DIM, BG);
}

fn main() {
    let mut window = Window::create_with_title(480, 400, "Monitor");
    let mut fb = window.framebuffer();
    let font_data = std::fs::read("/initrd/JetBrainsMono-8x16.font").expect("failed to read font");
    let font = font::Font::from_prebuilt(&font_data);

    // Initial render
    let info = query_sysinfo();
    render(&fb, &font, &info);
    window.present();

    loop {
        match window.recv_event() {
            window::Event::KeyInput(_) => {
                let info = query_sysinfo();
                render(&fb, &font, &info);
                window.present();
            }
            window::Event::Resized => {
                fb = window.framebuffer();
                let info = query_sysinfo();
                render(&fb, &font, &info);
                window.present();
            }
            window::Event::Close => break,
            _ => {}
        }
    }
}
