//! 虚拟键盘 / 虚拟鼠标组件(可视化主题的核心)。
//!
//! 作用:
//!   1. **展示**:画出一张标准美式键盘(含小键盘与导航键区)和一只虚拟鼠标,
//!      已被绑定的物理键"亮起"(语义色),未绑定的键保持立体但接近透明;
//!   2. **取键**:点击某个键 = 选中该物理键(空闲键进入截图取点流程,
//!      已绑定键弹出编辑面板) —— 取代旧的"先点[新增]再按键"的取键方式。
//!
//! 键码全部使用与全局捕获一致的 evdev 码空间(见 keymap::key_name),
//! 所以这里点出来的键和真实按下的键是同一个东西。
//!
//! 3D 立体感:每个键画两层 —— 先画往下偏移几像素的"前立面"(深色),
//! 再画"顶面"(本色),像键帽从键盘里凸出来。未绑定的键 alpha 压得很低,
//! 于是"整张键盘浮在界面上、设过的键亮着"。

use egui::{Color32, Pos2, Rect, Sense, Vec2};

/// 一个物理键的布局描述。`code == 0` 表示占位空隙(不可点击)。
#[derive(Clone, Copy)]
pub struct VkKey {
    pub label: &'static str,
    pub code: u16,
    /// 宽度(单位 = 一个标准键宽)
    pub w: f32,
    /// 高度倍数(2.0 = 跨两行,小键盘的 + / Enter 用)
    pub h: f32,
}

const fn k(label: &'static str, code: u16) -> VkKey {
    VkKey {
        label,
        code,
        w: 1.0,
        h: 1.0,
    }
}

const fn kw(label: &'static str, code: u16, w: f32) -> VkKey {
    VkKey {
        label,
        code,
        w,
        h: 1.0,
    }
}

const fn kh(label: &'static str, code: u16, w: f32, h: f32) -> VkKey {
    VkKey { label, code, w, h }
}

const fn gap(w: f32) -> VkKey {
    VkKey {
        label: "",
        code: 0,
        w,
        h: 1.0,
    }
}

/// 主键区(F 行 + 数字行 + 三行字母 + 修饰键行)
const MAIN_ROWS: &[&[VkKey]] = &[
    &[
        k("Esc", 1),
        gap(0.4),
        kw("F1", 59, 0.85),
        k("F2", 60),
        k("F3", 61),
        k("F4", 62),
        gap(0.4),
        kw("F5", 63, 0.85),
        k("F6", 64),
        k("F7", 65),
        k("F8", 66),
        gap(0.4),
        kw("F9", 67, 0.85),
        k("F10", 68),
        k("F11", 87),
        k("F12", 88),
        gap(0.4),
        kw("PrtSc", 99, 0.85),
        k("ScrLk", 70),
        k("Pause", 119),
    ],
    &[
        k("`", 41),
        k("1", 2),
        k("2", 3),
        k("3", 4),
        k("4", 5),
        k("5", 6),
        k("6", 7),
        k("7", 8),
        k("8", 9),
        k("9", 10),
        k("0", 11),
        k("-", 12),
        k("=", 13),
        kw("Backspace", 14, 2.0),
    ],
    &[
        kw("Tab", 15, 1.5),
        k("Q", 16),
        k("W", 17),
        k("E", 18),
        k("R", 19),
        k("T", 20),
        k("Y", 21),
        k("U", 22),
        k("I", 23),
        k("O", 24),
        k("P", 25),
        k("[", 26),
        k("]", 27),
        kw("\\", 43, 1.5),
    ],
    &[
        kw("Caps", 58, 1.75),
        k("A", 30),
        k("S", 31),
        k("D", 32),
        k("F", 33),
        k("G", 34),
        k("H", 35),
        k("J", 36),
        k("K", 37),
        k("L", 38),
        k(";", 39),
        k("'", 40),
        kw("Enter", 28, 2.25),
    ],
    &[
        kw("Shift", 42, 2.25),
        k("Z", 44),
        k("X", 45),
        k("C", 46),
        k("V", 47),
        k("B", 48),
        k("N", 49),
        k("M", 50),
        k(",", 51),
        k(".", 52),
        k("/", 53),
        kw("Shift", 54, 2.75),
    ],
    &[
        kw("Ctrl", 29, 1.25),
        kw("Win", 125, 1.25),
        kw("Alt", 56, 1.25),
        kw("Space", 57, 6.25),
        kw("Alt", 100, 1.25),
        kw("Win", 126, 1.25),
        kw("Menu", 127, 1.25),
        kw("Ctrl", 97, 1.25),
    ],
];

/// 导航键区(键盘右上那 3×3+1)
const NAV_ROWS: &[&[VkKey]] = &[
    &[k("Ins", 110), k("Home", 102), k("PgUp", 104)],
    &[k("Del", 111), k("End", 107), k("PgDn", 109)],
    &[gap(1.0), k("↑", 103), gap(1.0)],
    &[k("←", 105), k("↓", 108), k("→", 106)],
];

/// 小键盘区(4 列;+ 与 Enter 跨两行)
const NUM_ROWS: &[&[VkKey]] = &[
    &[k("Num", 69), k("/", 98), k("*", 55), k("-", 74)],
    &[k("7", 71), k("8", 72), k("9", 73), kh("+", 78, 1.0, 2.0)],
    &[k("4", 75), k("5", 76), k("6", 77)],
    &[k("1", 79), k("2", 80), k("3", 81), kh("Ent", 96, 1.0, 2.0)],
    &[kw("0", 82, 2.0), k(".", 83)],
];

/// 一个键的绘制参数(由调用方按"这个键绑了什么"决定)。
#[derive(Clone)]
pub struct KeyLook {
    /// 键帽顶面填充(含 alpha:未绑定=半透明,已绑定=语义色)
    pub fill: Color32,
    /// 键帽前立面/侧壁(比顶面更暗,画在顶面下方做出键帽厚度)
    pub side: Color32,
    /// 键帽外描边(要"鲜明一点",否则半透明的空键位贴在背景上根本看不出来)
    pub stroke: Color32,
    /// 标签文字色
    pub text: Color32,
    /// 悬停提示(键位详情,由调用方拼好)
    pub tip: Option<String>,
}

impl KeyLook {
    /// 未绑定的键:**老式机械键帽的空壳**。
    ///
    /// 三个硬要求(用户明确提过):
    ///   1. 底色与背景**不一样**——中灰蓝半透明,浅底/深底上都看得出来;
    ///   2. 比"背景色"更深、更像一块塑料键帽,而不是一层几乎看不见的雾;
    ///   3. 透明度很高的时候靠**鲜明的一圈描边**把轮廓立住。
    /// 颜色是固定的(不随底色调):半透明中灰在浅色与深色底上都能形成差别,
    /// 再配侧壁(更深)+ 描边(更亮)就有"键帽从壳里凸出来"的立体感。
    pub fn idle() -> Self {
        Self {
            fill: Color32::from_rgba_unmultiplied(122, 132, 150, 132),
            side: Color32::from_rgba_unmultiplied(58, 66, 82, 224),
            stroke: Color32::from_rgba_unmultiplied(184, 194, 212, 216),
            text: Color32::from_rgba_unmultiplied(238, 242, 250, 232),
            tip: None,
        }
    }
}

/// 键盘布局的总宽(单位数)= 主区 + 导航区 + 小键盘三块之和。
/// 主区取**最宽的一行**:F 行(12 个 1u 键 + 4 个 0.85u 功能键 + 4 个 0.4u 空隙 = 17u);
/// 块间的横向空隙是像素级的,在 show_keyboard 里按 gap 像素另加,不占单位。
/// (改动上方布局行时必须同步这里 —— 有单测锁死两者的关系。)
const TOTAL_UNITS: f32 = 17.0 + 3.0 + 4.0;

/// 画一张完整键盘;返回本帧被点击的键码。
///
/// `look_of`:按键码给出绘制参数(亮/暗/选中/取点闪烁等全由调用方决定)。
/// `pulse`:取点中键的闪烁相位(0..1),传 ui.time 即可;None = 不闪。
/// `reserve_right`:这一行右边还要放别的东西(虚拟鼠标)时,给它预留的像素宽 ——
/// 键盘按"可用宽度 − 预留"换算键宽,免得鼠标被挤出可视区(视觉验证时发现)。
pub fn show_keyboard(
    ui: &mut egui::Ui,
    id: &str,
    look_of: &dyn Fn(u16) -> KeyLook,
    pulse: Option<f64>,
    reserve_right: f32,
) -> Option<u16> {
    let gap = 3.0;
    let avail = (ui.available_width() - reserve_right).max(240.0);
    // 键宽 = (可用宽度 - 块间空隙 - 外壳留边) / 总单位数;太小则允许横向滚动
    let pad_x = 8.0; // 左右外壳留边
    let pad_top = 10.0;
    let pad_bottom = 12.0;
    let unit = ((avail - 4.0 * gap - 2.0 * pad_x) / TOTAL_UNITS).clamp(20.0, 44.0);
    let key_h = unit * 0.92;
    let depth = unit * 0.10; // 3D 前立面高度

    let want_w = TOTAL_UNITS * unit + 4.0 * gap + 2.0 * pad_x;
    let rows = 6usize;
    let want_h = rows as f32 * (key_h + gap) + depth + pad_top + pad_bottom;

    let scroll = egui::ScrollArea::horizontal()
        .id_salt(format!("{id}_scroll"))
        .show(ui, |ui| {
            let (rect, _resp) = ui.allocate_exact_size(Vec2::new(want_w, want_h), Sense::hover());
            // 键盘外壳(底盘):整张键盘坐在一块深色机身里,键帽因此"嵌在壳里"，
            // 这是"老式机械键盘"最关键的一层(没有它,键帽会像浮在界面上)。
            let chassis = egui::CornerRadius::same(12);
            let painter = ui.painter();
            painter.rect_filled(
                rect,
                chassis,
                Color32::from_rgba_unmultiplied(26, 29, 36, 196),
            );
            painter.rect_stroke(
                rect,
                chassis,
                egui::Stroke::new(1.0, Color32::from_rgba_unmultiplied(126, 136, 158, 150)),
                egui::StrokeKind::Middle,
            );
            let mut clicked = None;
            // 三块:主区 / 导航区 / 小键盘(从外壳留边开始摆)
            let mut x0 = rect.min.x + pad_x;
            let y0 = rect.min.y + pad_top;
            for block in [MAIN_ROWS, NAV_ROWS, NUM_ROWS] {
                draw_block(
                    ui,
                    Pos2::new(x0, y0),
                    block,
                    unit,
                    key_h,
                    gap,
                    depth,
                    look_of,
                    pulse,
                    &mut clicked,
                );
                x0 += block_units(block) * unit + 2.0 * gap;
            }
            clicked
        });
    scroll.inner
}

/// 计算一个块的总宽(单位数)
fn block_units(block: &[&[VkKey]]) -> f32 {
    block
        .iter()
        .map(|row| row.iter().map(|k| k.w).sum::<f32>())
        .fold(0.0_f32, f32::max)
}

/// 画一个键区块(支持跨行键:occupancy 网格,分辨率 0.25 单位)
#[allow(clippy::too_many_arguments)]
fn draw_block(
    ui: &mut egui::Ui,
    origin: Pos2,
    block: &[&[VkKey]],
    unit: f32,
    key_h: f32,
    gap: f32,
    depth: f32,
    look_of: &dyn Fn(u16) -> KeyLook,
    pulse: Option<f64>,
    clicked: &mut Option<u16>,
) {
    let cols = (block_units(block) / 0.25).ceil() as usize + 1;
    // occupancy[r][c]:第 r 行、第 c 个 0.25 单位列是否已被跨行键占住
    let mut occ = vec![vec![false; cols]; block.len() + 2];

    for (r, row) in block.iter().enumerate() {
        // 当前行的光标(0.25 单位列)
        let mut c = 0usize;
        for key in row.iter() {
            let wq = (key.w / 0.25).round() as usize;
            // 找一段没被占住的宽度
            while c + wq <= occ[r].len() && occ[r][c..c + wq].iter().any(|&b| b) {
                c += 1;
            }
            let x = origin.x + c as f32 * 0.25 * unit;
            let y = origin.y + r as f32 * (key_h + gap);
            // 跨行键(小键盘 + / Enter)把后面行的对应列也占住
            let span_rows = (key.h.ceil() as usize).max(1);
            for rr in r..r + span_rows {
                for cc in c..(c + wq).min(cols) {
                    if rr < occ.len() && cc < occ[rr].len() {
                        occ[rr][cc] = true;
                    }
                }
            }
            if key.code != 0 {
                let rect = Rect::from_min_size(
                    Pos2::new(x, y),
                    Vec2::new(key.w * unit - gap, key_h * key.h + gap * (key.h - 1.0)),
                );
                if draw_key(ui, rect, key, depth, look_of, pulse) {
                    *clicked = Some(key.code);
                }
            }
            c += wq;
        }
    }
}

/// 画一个键帽(老式机械键帽:裙边 + 顶面 + 内圈倒角 + 鲜明外框)。返回是否被点击。
fn draw_key(
    ui: &mut egui::Ui,
    rect: Rect,
    key: &VkKey,
    depth: f32,
    look_of: &dyn Fn(u16) -> KeyLook,
    pulse: Option<f64>,
) -> bool {
    let look = look_of(key.code);
    let fill = pulse_mask(look.fill, pulse);

    let painter = ui.painter();
    let round = egui::CornerRadius::same(4);
    // 1) 键帽裙边(侧壁):整体下沉 depth 并微微外扩 —— 键帽从外壳里"长出来"
    painter.rect_filled(
        rect.expand(0.7).translate(Vec2::new(0.0, depth)),
        round,
        pulse_mask(look.side, pulse),
    );
    // 2) 顶面
    painter.rect_filled(rect, round, fill);
    // 3) 内圈倒角:内缩一圈的浅色描边(键帽四边的斜切反光),机械键帽的"脸"
    let inner = rect.shrink(1.7);
    if inner.width() > 5.0 && inner.height() > 5.0 {
        painter.rect_stroke(
            inner,
            egui::CornerRadius::same(3),
            egui::Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 255, 255, 52)),
            egui::StrokeKind::Middle,
        );
    }
    // 4) 外框(半透明键位靠它立住轮廓)
    painter.rect_stroke(
        rect,
        round,
        egui::Stroke::new(1.2, look.stroke),
        egui::StrokeKind::Middle,
    );
    // 5) 标签(键宽足够才画,免得 F 行小键溢出)
    if label_fits(rect, key.label) {
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            key.label,
            egui::FontId::proportional((rect.height() * 0.34).clamp(9.0, 15.0)),
            look.text,
        );
    }
    let resp = ui.allocate_rect(rect, Sense::click());
    let resp = match look.tip {
        Some(tip) => resp.on_hover_text(tip),
        None => resp,
    };
    resp.clicked()
}

fn label_fits(rect: Rect, label: &str) -> bool {
    // 粗略估宽:每字符 ≈ 0.62 × 字号
    let font = (rect.height() * 0.34).clamp(9.0, 15.0);
    label.chars().count() as f32 * font * 0.62 <= rect.width() + 2.0
}

// ============================ 虚拟鼠标 ============================

/// 画一只虚拟鼠标(键盘右侧);返回被点击的键码(与全局捕获同码空间:
/// 左272/右273/中274/侧1 275/侧2 276/滚轮上277/滚轮下278)。
///
/// 布局示意:
/// ```text
///   ┌────┬────┐
///   │ 左 │ 右 │   上半:左右键
///   ├──┬─┴┬──┤
///   │侧│滚轮│  │   中部:侧键(左缘) + 滚轮上下
///   │键│    │  │
///   └──┴────┘
/// ```
pub fn show_mouse(
    ui: &mut egui::Ui,
    // 与 show_keyboard 保持同一调用形态(鼠标没有滚动区,不需要 id 做盐)
    _id: &str,
    look_of: &dyn Fn(u16) -> KeyLook,
    pulse: Option<f64>,
) -> Option<u16> {
    let w = 86.0;
    let h = 132.0;
    let (rect, _resp) = ui.allocate_exact_size(Vec2::new(w, h), Sense::hover());
    let painter = ui.painter().clone();
    // 鼠标壳体:与键盘同一套"深色机身 + 鲜明描边"的语言(空壳也看得见)
    let shell = KeyLook::idle();
    let round = egui::CornerRadius::same(16);
    painter.rect_filled(
        rect.translate(Vec2::new(0.0, 4.0)),
        round,
        Color32::from_rgba_unmultiplied(18, 20, 26, 216),
    );
    painter.rect_filled(rect, round, shell.fill);
    painter.rect_stroke(
        rect,
        round,
        egui::Stroke::new(1.2, shell.stroke),
        egui::StrokeKind::Middle,
    );

    let mut clicked = None;
    // 画一个鼠标部件(同键盘的键帽画法:裙边 + 顶面 + 倒角 + 外框;点击则记录键码)。
    // clicked 用参数传入传出:闭包捕获 + Response 的移动语义搅在一起会出借用问题。
    #[allow(clippy::too_many_arguments)]
    fn part(
        ui: &mut egui::Ui,
        painter: &egui::Painter,
        look_of: &dyn Fn(u16) -> KeyLook,
        pulse: Option<f64>,
        r: Rect,
        label: &str,
        code: u16,
        clicked: &mut Option<u16>,
    ) {
        let l = look_of(code);
        let fill = pulse_mask(l.fill, pulse);
        let rr = egui::CornerRadius::same(4);
        painter.rect_filled(
            r.expand(0.6).translate(Vec2::new(0.0, 2.0)),
            rr,
            pulse_mask(l.side, pulse),
        );
        painter.rect_filled(r, rr, fill);
        let inner = r.shrink(1.5);
        if inner.width() > 4.0 && inner.height() > 4.0 {
            painter.rect_stroke(
                inner,
                egui::CornerRadius::same(3),
                egui::Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 255, 255, 46)),
                egui::StrokeKind::Middle,
            );
        }
        painter.rect_stroke(
            r,
            rr,
            egui::Stroke::new(1.2, l.stroke),
            egui::StrokeKind::Middle,
        );
        painter.text(
            r.center(),
            egui::Align2::CENTER_CENTER,
            label,
            egui::FontId::proportional(10.0),
            l.text,
        );
        let resp = ui.allocate_rect(r, Sense::click());
        let resp = match l.tip {
            Some(tip) => resp.on_hover_text(tip),
            None => resp,
        };
        if resp.clicked() {
            *clicked = Some(code);
        }
    }

    let top = rect.min.y;
    let mid = rect.min.y + h * 0.42;
    let inset = 5.0;
    // 左右键
    let half = (w - inset * 3.0 - 6.0) / 2.0;
    part(
        ui,
        &painter,
        look_of,
        pulse,
        Rect::from_min_size(
            Pos2::new(rect.min.x + inset, top + inset),
            Vec2::new(half, h * 0.30),
        ),
        "左",
        272,
        &mut clicked,
    );
    part(
        ui,
        &painter,
        look_of,
        pulse,
        Rect::from_min_size(
            Pos2::new(rect.max.x - inset - half, top + inset),
            Vec2::new(half, h * 0.30),
        ),
        "右",
        273,
        &mut clicked,
    );
    // 中键(滚轮条)
    part(
        ui,
        &painter,
        look_of,
        pulse,
        Rect::from_min_size(
            Pos2::new(rect.center().x - 8.0, top + inset + 6.0),
            Vec2::new(16.0, h * 0.16),
        ),
        "中",
        274,
        &mut clicked,
    );
    // 滚轮上下(小箭头,放滚轮条下缘两侧)
    part(
        ui,
        &painter,
        look_of,
        pulse,
        Rect::from_min_size(
            Pos2::new(rect.min.x + inset + 2.0, mid - 2.0),
            Vec2::new(w * 0.4, h * 0.10),
        ),
        "滚上",
        277,
        &mut clicked,
    );
    part(
        ui,
        &painter,
        look_of,
        pulse,
        Rect::from_min_size(
            Pos2::new(rect.max.x - inset - 2.0 - w * 0.4, mid - 2.0),
            Vec2::new(w * 0.4, h * 0.10),
        ),
        "滚下",
        278,
        &mut clicked,
    );
    // 侧键 1/2(鼠标左缘)
    part(
        ui,
        &painter,
        look_of,
        pulse,
        Rect::from_min_size(
            Pos2::new(rect.min.x - 4.0, mid + h * 0.10),
            Vec2::new(14.0, h * 0.13),
        ),
        "侧1",
        275,
        &mut clicked,
    );
    part(
        ui,
        &painter,
        look_of,
        pulse,
        Rect::from_min_size(
            Pos2::new(rect.min.x - 4.0, mid + h * 0.26),
            Vec2::new(14.0, h * 0.13),
        ),
        "侧2",
        276,
        &mut clicked,
    );
    clicked
}

fn pulse_mask(c: Color32, pulse: Option<f64>) -> Color32 {
    match pulse {
        Some(t) => {
            let k = (t.sin() * 0.5 + 0.5) as f32;
            Color32::from_rgba_unmultiplied(
                c.r(),
                c.g(),
                c.b(),
                (c.a() as f32 * (0.45 + 0.55 * k)) as u8,
            )
        }
        None => c,
    }
}

/// 鼠标部件清单(悬停提示/操作面板枚举用;当前 UI 按码点直接绘制,清单留作枚举依据)
#[allow(dead_code)]
pub const MOUSE_CODES: [u16; 7] = [272, 273, 274, 275, 276, 277, 278];

#[cfg(test)]
mod tests {
    use super::*;

    /// 布局必须是"矩形能装下"的:同一行内的键不得重叠(占位网格算法的最基本回归)。
    /// 这里用纯数据验证:每行按宽度累加,结果不超过该块最大宽 + 一点容差。
    #[test]
    fn rows_do_not_exceed_block_width() {
        for block in [MAIN_ROWS, NAV_ROWS, NUM_ROWS] {
            let max_w = block_units(block);
            for row in block {
                let sum: f32 = row.iter().map(|k| k.w).sum();
                assert!(
                    sum <= max_w + 1e-3,
                    "行宽 {sum} 超过块宽 {max_w}(块内行必须等宽)"
                );
            }
        }
    }

    /// 键码必须非零且不重复(重复 = 点同一个键有两个身份,取键语义就乱了)
    #[test]
    fn key_codes_unique() {
        let mut seen = std::collections::HashSet::new();
        for block in [MAIN_ROWS, NAV_ROWS, NUM_ROWS] {
            for row in block {
                for k in row.iter() {
                    if k.code != 0 {
                        assert!(seen.insert(k.code), "键码 {} 重复", k.code);
                    }
                }
            }
        }
        // 鼠标键也不与键盘键冲突
        for c in MOUSE_CODES {
            assert!(seen.insert(c), "鼠标键码 {c} 与键盘键冲突");
        }
    }

    /// 常用绑定键(WASD/空格/Shift/Ctrl)必须出现在键盘上 ——
    /// 否则用户已有的配置在可视化界面里"看不见"
    #[test]
    fn common_gaming_keys_present() {
        let mut codes = std::collections::HashSet::new();
        for block in [MAIN_ROWS, NAV_ROWS, NUM_ROWS] {
            for row in block {
                for k in row.iter() {
                    codes.insert(k.code);
                }
            }
        }
        for c in [17u16, 31, 30, 32, 57, 42, 29, 54, 28, 14, 15, 58] {
            assert!(codes.contains(&c), "常用键 {c} 缺失");
        }
        // 小键盘数字
        for c in [71u16, 75, 79, 82] {
            assert!(codes.contains(&c), "小键盘键 {c} 缺失");
        }
    }

    /// 三个键区的标准宽度:主区 17(F 行最宽)/ 导航 3 / 小键盘 4(单位 = 一个标准键宽)。
    /// 这是"看起来像一张真的美式键盘"的比例基准,改动布局行时必须一起更新。
    #[test]
    fn blocks_keep_standard_widths() {
        let near = |a: f32, b: f32| (a - b).abs() < 1e-3;
        assert!(near(block_units(MAIN_ROWS), 17.0), "主键区应为 17 单位");
        assert!(near(block_units(NAV_ROWS), 3.0), "导航区应为 3 单位");
        assert!(near(block_units(NUM_ROWS), 4.0), "小键盘应为 4 单位");
    }

    /// TOTAL_UNITS 是单元级的硬编码(用于按可用宽度换算键宽),必须与三块布局严格同步:
    /// 布局行改了、这里忘了改,键盘就会按错误比例绘制 —— 典型症状是小键盘右列被裁掉
    /// (曾经的真实 bug:23.55 < 24,右侧 0.45 单位画到滚动区外面)。
    #[test]
    fn total_units_tracks_block_widths() {
        let sum = block_units(MAIN_ROWS) + block_units(NAV_ROWS) + block_units(NUM_ROWS);
        assert!(
            (TOTAL_UNITS - sum).abs() < 0.01,
            "TOTAL_UNITS={TOTAL_UNITS} 与布局宽度 {sum} 不同步,请同步修改"
        );
    }

    /// 跨行键只有小键盘的 + 与 Enter(2 行高);其余键一律 1 行高。
    /// 跨行靠 occupancy 网格定位,比例错了就会压到别的键上。
    #[test]
    fn only_numpad_plus_and_enter_span_rows() {
        let mut tall = Vec::new();
        for row in NUM_ROWS {
            for k in row.iter() {
                if k.h > 1.0 {
                    tall.push((k.code, k.h));
                }
            }
        }
        assert_eq!(
            tall,
            vec![(78u16, 2.0), (96u16, 2.0)],
            "只有 + 与 Enter 跨两行"
        );
        for block in [MAIN_ROWS, NAV_ROWS] {
            for row in block {
                for k in row.iter() {
                    assert_eq!(k.h, 1.0, "主区/导航区不应有跨行键: {}", k.label);
                }
            }
        }
    }

    /// 未绑定的键必须"看得见但仍是半透明":底色与背景有差别、描边鲜明、侧壁更实。
    /// (旧版 alpha 14 几乎等于隐形,用户明确不满 —— 这里锁死"最低可见度"。)
    #[test]
    fn idle_keys_are_visible_but_translucent() {
        let l = KeyLook::idle();
        assert!(
            (60..200).contains(&l.fill.a()),
            "空闲键填充应半透明可见(不能接近全透明,也不能盖死背景),实际 {}",
            l.fill.a()
        );
        assert!(
            l.stroke.a() >= 180,
            "空闲键描边要鲜明,实际 {}",
            l.stroke.a()
        );
        assert!(l.side.a() > l.fill.a(), "侧壁应比顶面更实,才能做出键帽厚度");
        assert!(l.tip.is_none());
        // 不闪时 pulse_mask 必须原样返回(取点中的呼吸闪烁只在传 Some 时发生)
        assert_eq!(pulse_mask(l.fill, None), l.fill);
    }
}
