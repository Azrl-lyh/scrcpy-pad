//! 使用说明窗口:把 [`help.md`] 渲染成 egui 界面,并带一份章节索引。
//!
//! markdown 解析交给轻量的纯 Rust 解析器 `pulldown-cmark`(只取解析结果,不用它的
//! HTML 输出),渲染由本模块自己实现 —— 这样字号、配色能跟着程序[外观]设置走,
//! 也不必为了一个说明窗口拖着整套 HTML 渲染进来。

use egui::text::{LayoutJob, TextFormat};
use egui::{Align, Color32, FontFamily, FontId, Response, RichText, TextStyle, Ui};
use pulldown_cmark::{Event, Parser, Tag, TagEnd};

use crate::theme::Theme;

/// 编译期嵌入的使用说明(markdown 原文)
const HELP_MD: &str = include_str!("help.md");

/// 左侧章节索引的宽度(含内边距),其余宽度留给正文
const INDEX_WIDTH: f32 = 210.0;

/// 索引里的一条章节
struct Section {
    /// 标题层级(1..=6),决定索引里的缩进
    level: u8,
    title: String,
    /// 第几个标题(从 1 开始),渲染正文时按它定位
    anchor: usize,
}

/// 使用说明窗口状态(由 app 持有,跨帧保留)
pub struct HelpWindow {
    sections: Vec<Section>,
    /// 待跳转的章节:点索引后置位,正文渲染到对应标题时消费掉
    jump: Option<usize>,
    /// 当前章节(索引里高亮的那条)
    current: usize,
}

impl Default for HelpWindow {
    fn default() -> Self {
        Self::new()
    }
}

impl HelpWindow {
    pub fn new() -> Self {
        Self {
            sections: parse_sections(HELP_MD),
            jump: None,
            current: 1,
        }
    }

    /// 每帧调用;`open` 为窗口开关(app 持有的字段)
    pub fn show(&mut self, ctx: &egui::Context, open: &mut bool, th: &Theme) {
        if !*open {
            return;
        }
        let mut opened = true;
        egui::Window::new("使用说明")
            .open(&mut opened)
            .default_width(900.0)
            .default_height(640.0)
            .resizable(true)
            .show(ctx, |ui| {
                // 左侧索引做成真正的子面板(Panel::show),**不要**用
                // ui.horizontal + allocate_ui 自己拼。原因是 egui 的换行策略跟布局
                // 方向绑定(Ui::wrap_mode):
                //   横向且不换行的布局 → TextWrapMode::Extend,
                // 而 Extend 会被 WidgetText::into_galley 用来**覆盖** LayoutJob 自带
                // 的 wrap.max_width。于是正文既不折行、一路向右无限延伸;同时横向
                // 布局里 available_height() 只有"光标那一行"的高度(≈0),索引区拿不到
                // 高度,条目点不动。
                // 换成 Panel 后:它把父 Ui 的可用区缩到右边剩余部分,正文因此在纵向
                // 布局里正常折行,并且拿到整窗高度。
                egui::Panel::left("help_index")
                    .resizable(false)
                    .exact_size(INDEX_WIDTH)
                    // 索引条的底色跟窗口一致:子面板默认取的是 `panel_fill`
                    // (主界面那层,背景图开着时更透),压在窗口里会显得比正文更"飘"。
                    .frame(egui::Frame::side_top_panel(ui.style()).fill(ui.visuals().window_fill))
                    .show(ui, |ui| {
                        ui.strong("章节");
                        ui.separator();
                        egui::ScrollArea::vertical()
                            .id_salt("help_index_scroll")
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                if let Some(a) = draw_index(ui, &self.sections, self.current) {
                                    self.jump = Some(a);
                                }
                            });
                    });
                // 右边就是正文,宽度=窗口剩余宽度
                egui::ScrollArea::vertical()
                    .id_salt("help_body")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        let mut r = Renderer::new(ui, th, self.jump.take(), self.current);
                        r.render(ui);
                        self.current = r.current;
                        // 目标标题已不存在时不要一直挂着,免得每帧都去滚动
                        self.jump = r.jump.filter(|a| *a <= r.heading_ord);
                    });
            });
        *open = opened;
    }
}

/// 从 markdown 抽标题做章节索引
fn parse_sections(md: &str) -> Vec<Section> {
    let mut out: Vec<Section> = Vec::new();
    let mut cur: Option<(u8, String)> = None;
    let mut ord = 0usize;
    for ev in Parser::new(md) {
        match ev {
            Event::Start(Tag::Heading { level, .. }) => {
                ord += 1;
                cur = Some((level as u8, String::new()));
            }
            Event::Text(t) => {
                if let Some((_, title)) = cur.as_mut() {
                    title.push_str(&t);
                }
            }
            Event::End(TagEnd::Heading(_)) => {
                if let Some((level, title)) = cur.take() {
                    out.push(Section {
                        level,
                        title,
                        anchor: ord,
                    });
                }
            }
            _ => {}
        }
    }
    out
}

/// 画章节索引;返回被点中的标题序号
fn draw_index(ui: &mut Ui, sections: &[Section], current: usize) -> Option<usize> {
    let mut clicked = None;
    for s in sections {
        // 缩进用全角空格前缀,而不是横向布局里的 add_space:纵向布局里 Label 才会
        // 折行,标题长了也不会伸到面板外面去
        let indent = "　".repeat(s.level.saturating_sub(1) as usize);
        let text = RichText::new(format!("{indent}{}", s.title));
        let text = if s.level <= 1 { text.strong() } else { text };
        if ui.selectable_label(s.anchor == current, text).clicked() {
            clicked = Some(s.anchor);
        }
    }
    clicked
}

/// 把 markdown 事件流铺成 egui 控件的渲染器
struct Renderer {
    /// 当前正在拼的一块文本(段落/列表项/标题)
    job: LayoutJob,
    strong: bool,
    emphasis: bool,
    /// Some = 当前在标题里,值是标题字号
    heading_size: Option<f32>,
    link: Option<String>,
    /// 列表栈:None=无序,Some(n)=有序的下一个序号
    lists: Vec<Option<u64>>,
    quote: usize,
    code_block: Option<String>,

    body_size: f32,
    body_color: Color32,
    strong_color: Color32,
    code_color: Color32,
    code_bg: Color32,
    muted: Color32,
    accent: Color32,

    heading_ord: usize,
    /// 当前标题的序号(用于跳转定位)
    cur_heading: usize,
    jump: Option<usize>,
    current: usize,
    /// 本帧是否已经确定"当前章节"
    current_done: bool,
}

impl Renderer {
    fn new(ui: &Ui, th: &Theme, jump: Option<usize>, current: usize) -> Self {
        let body_size = ui
            .style()
            .text_styles
            .get(&TextStyle::Body)
            .map(|f| f.size)
            .unwrap_or(14.0);
        Self {
            job: LayoutJob::default(),
            strong: false,
            emphasis: false,
            heading_size: None,
            link: None,
            lists: Vec::new(),
            quote: 0,
            code_block: None,
            body_size,
            body_color: ui.visuals().text_color(),
            strong_color: ui.visuals().strong_text_color(),
            code_color: th.ok,
            code_bg: ui.visuals().extreme_bg_color,
            muted: th.muted,
            accent: th.accent,
            heading_ord: 0,
            cur_heading: 0,
            jump,
            current,
            current_done: false,
        }
    }

    fn render(&mut self, ui: &mut Ui) {
        self.render_markdown(ui, HELP_MD);
    }

    /// 渲染任意 markdown 原文:生产固定传 [`HELP_MD`],测试用合成文本驱动同一套渲染
    /// 代码(不必为了验一条渲染规则去改真实的 help.md)。
    fn render_markdown(&mut self, ui: &mut Ui, md: &str) {
        for ev in Parser::new(md) {
            match ev {
                Event::Start(tag) => self.start(tag),
                Event::End(end) => self.end(ui, end),
                Event::Text(t) => {
                    // 代码块里的文本**不能**混进段落 job:混进去就会和前后正文挤在同一个
                    // LayoutJob 里、用正文字体渲染,用户看到的"命令"其实是一段正文,复制
                    // 出去也带着说明文字。先攒进 code_block,由 TagEnd::CodeBlock 单独铺一块
                    // 等宽字体 + 底色的文本块。
                    if let Some(buf) = self.code_block.as_mut() {
                        buf.push_str(&t);
                    } else {
                        let f = self.fmt(false);
                        self.job.append(&t, 0.0, f);
                    }
                }
                Event::Code(t) => {
                    let f = self.fmt(true);
                    self.job.append(&t, 0.0, f);
                }
                Event::SoftBreak => {
                    if let Some(buf) = self.code_block.as_mut() {
                        // 代码块内换行必须保留(改成空格会把多行命令粘成一行)
                        buf.push('\n');
                    } else {
                        let f = self.fmt(false);
                        self.job.append(" ", 0.0, f);
                    }
                }
                Event::HardBreak => {
                    if let Some(buf) = self.code_block.as_mut() {
                        buf.push('\n');
                    } else {
                        let f = self.fmt(false);
                        self.job.append("\n", 0.0, f);
                    }
                }
                Event::Rule => {
                    self.flush(ui);
                    ui.separator();
                }
                _ => {}
            }
        }
        self.flush(ui);
    }

    fn start(&mut self, tag: Tag) {
        match tag {
            Tag::Heading { level, .. } => {
                self.heading_ord += 1;
                self.cur_heading = self.heading_ord;
                self.heading_size = Some(match level as u8 {
                    1 => self.body_size + 10.0,
                    2 => self.body_size + 4.0,
                    3 => self.body_size + 1.5,
                    _ => self.body_size + 0.5,
                });
            }
            Tag::Paragraph => {
                if self.quote > 0 {
                    let f = self.fmt(false);
                    let mark = "▏ ".repeat(self.quote);
                    self.job.append(&mark, 0.0, f);
                }
            }
            Tag::Strong => self.strong = true,
            Tag::Emphasis => self.emphasis = true,
            Tag::Link { dest_url, .. } => self.link = Some(dest_url.to_string()),
            Tag::List(start) => self.lists.push(start),
            Tag::Item => {
                // 列表用缩进空格 + 项目符号前缀表示:egui 的 LayoutJob 没有悬挂缩进,
                // 这是最省事又不会串行的做法
                let depth = self.lists.len().saturating_sub(1);
                let prefix = match self.lists.last_mut() {
                    Some(Some(n)) => {
                        let cur = *n;
                        *n += 1;
                        format!("{cur}. ")
                    }
                    _ => "• ".to_string(),
                };
                let f = self.fmt(false);
                if depth > 0 {
                    self.job.append(&"    ".repeat(depth), 0.0, f.clone());
                }
                self.job.append(&prefix, 0.0, f);
            }
            Tag::CodeBlock(_) => self.code_block = Some(String::new()),
            Tag::BlockQuote(_) => self.quote += 1,
            _ => {}
        }
    }

    fn end(&mut self, ui: &mut Ui, end: TagEnd) {
        match end {
            TagEnd::Paragraph | TagEnd::Item => self.flush(ui),
            TagEnd::Heading(_) => {
                if let Some(resp) = self.flush_resp(ui) {
                    // 第一个下沿越过视口顶部的标题,就是"当前章节"
                    if !self.current_done && resp.rect.bottom() >= ui.clip_rect().top() {
                        self.current = self.cur_heading;
                        self.current_done = true;
                    }
                    if self.jump == Some(self.cur_heading) {
                        ui.scroll_to_rect(resp.rect, Some(Align::TOP));
                        self.current = self.cur_heading;
                        self.jump = None;
                    }
                }
                ui.add_space(4.0);
                self.heading_size = None;
            }
            TagEnd::List(_) => {
                self.lists.pop();
                ui.add_space(2.0);
            }
            TagEnd::CodeBlock => {
                if let Some(text) = self.code_block.take() {
                    let f = self.fmt(true);
                    let mut job = LayoutJob::default();
                    job.append(text.trim_end_matches('\n'), 0.0, f);
                    self.push_block(ui, job);
                    ui.add_space(2.0);
                }
            }
            TagEnd::Strong => self.strong = false,
            TagEnd::Emphasis => self.emphasis = false,
            TagEnd::Link => {
                if let Some(url) = self.link.clone() {
                    let f = self.fmt(false);
                    self.job.append(&format!(" ({url})"), 0.0, f);
                    self.link = None;
                }
            }
            TagEnd::BlockQuote(_) => self.quote = self.quote.saturating_sub(1),
            _ => {}
        }
    }

    /// 当前样式下的文本格式
    fn fmt(&self, code: bool) -> TextFormat {
        let base = self.heading_size.unwrap_or(self.body_size);
        // egui 没有真正的粗体字重(用的系统 CJK 字体是单一字重),
        // 用"略大一号 + 更强的前景色"来代替加粗
        let size = if self.strong { base + 1.0 } else { base };
        let family = if code {
            FontFamily::Monospace
        } else {
            FontFamily::Proportional
        };
        let color = if code {
            self.code_color
        } else if self.heading_size.is_some() {
            self.strong_color
        } else if self.link.is_some() {
            self.accent
        } else if self.quote > 0 {
            self.muted
        } else {
            self.body_color
        };
        TextFormat {
            font_id: FontId::new(size, family),
            color,
            italics: self.emphasis,
            background: if code {
                self.code_bg
            } else {
                Color32::TRANSPARENT
            },
            ..Default::default()
        }
    }

    fn flush(&mut self, ui: &mut Ui) {
        let _ = self.flush_resp(ui);
    }

    /// 把当前累积的一块文本贴出去;返回该块的 Response(供标题定位用)
    fn flush_resp(&mut self, ui: &mut Ui) -> Option<Response> {
        if self.job.text.is_empty() {
            return None;
        }
        let job = std::mem::take(&mut self.job);
        let resp = self.push_block(ui, job);
        ui.add_space(3.0);
        resp
    }

    fn push_block(&self, ui: &mut Ui, job: LayoutJob) -> Option<Response> {
        // 必须显式 .wrap():egui 会用当前布局推出的 TextWrapMode 覆盖
        // LayoutJob 里的 wrap 设置(见 WidgetText::into_galley),横向布局下那是
        // "不换行、一直向右";在 Label 上写死 Wrap 才保证正文按宽度排版。
        Some(ui.add(egui::Label::new(job).wrap()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{PointerButton, Rect, pos2, vec2};

    /// 窗口宽度(测试里的屏幕够大,窗口不会被挤)
    const WIN_W: f32 = 900.0;

    /// 无窗口跑一帧,收集这一帧所有文本块的 (文本, 屏幕矩形)
    fn run(
        ctx: &egui::Context,
        w: &mut HelpWindow,
        open: &mut bool,
        th: &Theme,
        events: Vec<egui::Event>,
    ) -> Vec<(String, Rect)> {
        // 时间必须往前走:egui 的滚动是带动画的,时间不动就永远滚不到位
        let time = ctx.input(|i| i.time) + 1.0 / 60.0;
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(1400.0, 900.0))),
            time: Some(time),
            events,
            ..Default::default()
        };
        // egui 0.36 里 Context::run 已换成 run_ui(只给一个根 Ui),这里要的是
        // 按 Context 起一帧(窗口是 Context 级的),所以用 begin_pass/end_pass
        ctx.begin_pass(input);
        w.show(ctx, open, th);
        let out = ctx.end_pass();
        let mut texts = Vec::new();
        for cs in &out.shapes {
            collect_text(&cs.shape, &mut texts);
        }
        // 测试里没有真实渲染后端,纹理增量没人接收,得显式丢掉
        out.drop_without_applying_deltas();
        texts
    }

    fn collect_text(shape: &egui::Shape, out: &mut Vec<(String, Rect)>) {
        match shape {
            egui::Shape::Text(t) => {
                out.push((
                    t.galley.text().to_string(),
                    t.galley.rect.translate(t.pos.to_vec2()),
                ));
            }
            egui::Shape::Vec(v) => {
                for s in v {
                    collect_text(s, out);
                }
            }
            _ => {}
        }
    }

    /// 在索引那一栏里找文本(索引贴左,标题带全角空格缩进,所以用后缀匹配)
    fn find_in_index(texts: &[(String, Rect)], text: &str) -> Option<Rect> {
        texts
            .iter()
            .find(|(t, r)| r.min.x < INDEX_WIDTH && t.ends_with(text))
            .map(|(_, r)| *r)
    }

    /// 在正文那一栏里找文本
    fn find_in_body(texts: &[(String, Rect)], text: &str) -> Option<Rect> {
        texts
            .iter()
            .find(|(t, r)| r.min.x > INDEX_WIDTH && t == text)
            .map(|(_, r)| *r)
    }

    fn click(ev: &mut Vec<egui::Event>, pos: egui::Pos2, pressed: bool) {
        ev.push(egui::Event::PointerButton {
            pos,
            button: PointerButton::Primary,
            pressed,
            modifiers: Default::default(),
        });
    }

    /// 正文必须按窗口宽度折行。
    ///
    /// 这条是回归测试:早先索引与正文是用 `ui.horizontal` + `allocate_ui` 拼的,
    /// 而不换行的横向布局里 `Ui::wrap_mode()` 是 `TextWrapMode::Extend`,且它会被
    /// 用来覆盖 `LayoutJob` 自带的 `wrap.max_width` —— 结果是每个段落都只占一行、
    /// 一路向右无限延伸(表现为"只有第一行有字")。
    #[test]
    fn help_body_wraps_to_window_width() {
        let ctx = egui::Context::default();
        let th = Theme::dark();
        let mut w = HelpWindow::new();
        let mut open = true;

        let mut texts = Vec::new();
        for _ in 0..5 {
            texts = run(&ctx, &mut w, &mut open, &th, Vec::new());
        }

        // egui 会跳过视口外的 Label(见 widgets/label.rs 里的 is_rect_visible),
        // 所以能收集到的就是"首屏"画出来的字
        let body: Vec<&(String, Rect)> = texts
            .iter()
            .filter(|(_, r)| r.min.x > INDEX_WIDTH)
            .collect();
        assert!(!body.is_empty(), "正文一个字都没画出来");

        let widest = body.iter().map(|(_, r)| r.width()).fold(0.0f32, f32::max);
        assert!(
            widest < WIN_W,
            "正文没有按窗口宽度折行:最宽的一行有 {widest:.0}px,而窗口只有 {WIN_W}px 宽"
        );
        // 索引和正文各占一块,不能叠在一起
        let index_right = texts
            .iter()
            .filter(|(_, r)| r.min.x < INDEX_WIDTH)
            .map(|(_, r)| r.max.x)
            .fold(0.0f32, f32::max);
        let body_left = body.iter().map(|(_, r)| r.min.x).fold(f32::MAX, f32::min);
        assert!(
            index_right < body_left,
            "索引({index_right:.0}px)压到了正文({body_left:.0}px)上"
        );
    }

    /// 点左侧索引条目要能把正文滚到对应章节。
    ///
    /// 回归点:横向布局里 `available_height()` 只有"光标那一行"的高度(≈0),
    /// 索引区因此拿不到高度、条目点不动。
    #[test]
    fn clicking_index_entry_scrolls_body() {
        let ctx = egui::Context::default();
        let th = Theme::dark();
        let mut w = HelpWindow::new();
        let mut open = true;

        for _ in 0..5 {
            run(&ctx, &mut w, &mut open, &th, Vec::new());
        }

        // 取"索引里**当前可见**的最靠后章节":离正文顶部最远,跳转效果最明显。
        //
        // 两条边界都是踩过的坑:
        //  1. 不能直接取整份文档的最后一节 —— 说明越写越长(2026-10-26 新增「Linux 上
        //     做不到的几项」一节)后,末条索引被顶到窗口下沿乃至窗口外。ScrollArea
        //     里的行**仍会被画出来**(内容不裁),但点击送不到,于是测试误报"没滚"。
        //  2. 也不能只按"在 900 高的测试屏幕内"筛 —— 窗口默认高 640(见 show() 的
        //     default_height),y>=640 的行已经在窗口外:首次修这条测试时上界写 880,
        //     选中的那条落在 y=655 ⇒ 点击无效、正文没动,如实报红(不是假红)。
        // 所以上界必须落在窗口内,并给下沿留余量(避免整行被半裁、中心点在裁剪矩形外):
        const SAFE_TOP: f32 = 60.0;
        const SAFE_BOTTOM: f32 = 520.0;
        let texts = run(&ctx, &mut w, &mut open, &th, Vec::new());
        let sec = w
            .sections
            .iter()
            .rev()
            .find_map(|s| {
                let r = find_in_index(&texts, &s.title)?;
                (r.center().y > SAFE_TOP && r.center().y < SAFE_BOTTOM).then(|| s.title.clone())
            })
            .expect("索引里应至少有一条落在可视区内的章节");
        let idx = find_in_index(&texts, &sec).expect("索引里找不到选中的章节");
        // 视口外的文字不会被画出来,正好用来确认这一节本来在下面
        assert!(
            find_in_body(&texts, &sec).is_none(),
            "测试前提不成立:{sec} 一开始就在正文视口里"
        );

        // 按下、抬起分两帧,跟真实点击一致
        let p = idx.center();
        run(
            &ctx,
            &mut w,
            &mut open,
            &th,
            vec![egui::Event::PointerMoved(p)],
        );
        let mut ev = Vec::new();
        click(&mut ev, p, true);
        run(&ctx, &mut w, &mut open, &th, ev);
        let mut ev = Vec::new();
        click(&mut ev, p, false);
        run(&ctx, &mut w, &mut open, &th, ev);

        // 滚动带动画,而且 egui 要到下一两帧才消费掉滚动目标,多跑几帧等它停
        let mut texts = Vec::new();
        for _ in 0..40 {
            texts = run(&ctx, &mut w, &mut open, &th, Vec::new());
        }
        let r = find_in_body(&texts, &sec).expect("点索引之后正文没有滚到该章节");
        assert!(
            r.min.y >= 0.0 && r.min.y < 640.0,
            "该章节标题停在 y={:.0},不在正文视口里",
            r.min.y
        );
        // 而且得是真滚了:正文开头那一屏应该已经被顶出去
        assert!(
            find_in_body(&texts, "scrcpy-pad 使用说明").is_none(),
            "点索引之后正文几乎没有滚动"
        );
        // 索引高亮(当前章节=视口最上面那一节)也得跟着走
        assert!(w.current > 1, "当前章节没有跟着跳转,仍是 {}", w.current);
    }

    /// 无窗口直接渲染一段 markdown,收集这一帧所有文本块的 (文本, 矩形, 字体族, 底色)
    ///
    /// 需要这个入口是因为"代码块该用等宽字体单独铺一块"这条规则得用**合成 markdown**
    /// 才能稳定验到:真实 help.md 里那块命令在长文档的中后段,没有滚动就画不到视口里,
    /// 测试会变成"看运气"。
    fn run_md(
        ctx: &egui::Context,
        th: &Theme,
        md: &str,
    ) -> Vec<(String, Rect, egui::FontFamily, egui::Color32)> {
        // 窗口第一帧只做"我要多大"的登记,尺寸要到下一帧才生效;第一帧里内容
        // 还没拿到可用区,Label 会被判成不可见而不画。所以跑 3 帧,收最后一帧。
        let mut texts = Vec::new();
        for _ in 0..3 {
            let time = ctx.input(|i| i.time) + 1.0 / 60.0;
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(1400.0, 900.0))),
                time: Some(time),
                ..Default::default()
            };
            ctx.begin_pass(input);
            egui::Window::new("md_test")
                .default_width(600.0)
                .show(ctx, |ui| {
                    let mut r = Renderer::new(ui, th, None, 0);
                    r.render_markdown(ui, md);
                });
            let out = ctx.end_pass();
            texts.clear();
            for cs in &out.shapes {
                collect_text_fmt(&cs.shape, &mut texts);
            }
            out.drop_without_applying_deltas();
        }
        texts
    }

    fn collect_text_fmt(
        shape: &egui::Shape,
        out: &mut Vec<(String, Rect, egui::FontFamily, egui::Color32)>,
    ) {
        match shape {
            egui::Shape::Text(t) => {
                // 取第一段的格式:本模块里一块文本内部格式一致(粗体/链接除外)
                let sec = t.galley.job.sections.first();
                let family = sec
                    .map(|s| s.format.font_id.family.clone())
                    .unwrap_or(egui::FontFamily::Proportional);
                let bg = sec
                    .map(|s| s.format.background)
                    .unwrap_or(egui::Color32::TRANSPARENT);
                out.push((
                    t.galley.text().to_string(),
                    t.galley.rect.translate(t.pos.to_vec2()),
                    family,
                    bg,
                ));
            }
            egui::Shape::Vec(v) => {
                for s in v {
                    collect_text_fmt(s, out);
                }
            }
            _ => {}
        }
    }

    /// 围栏代码块必须单独铺成等宽字体的一块,不能混进正文段落。
    ///
    /// 回归点(2026-10-26):`Event::Text` 原先无条件 append 到段落 job,于是 help.md
    /// 里新加的 bash 命令块被按正文字体、和前后说明文字挤在同一段里渲染 —— 用户看到的
    /// "可复制命令"其实是一段正文。help.md 在这之前一个代码块都没有,这条路径从未
    /// 被走过,所以一直是坏的而没人发现。
    #[test]
    fn fenced_code_blocks_render_separately_in_monospace() {
        // 生产文档里必须真有代码块,否则这条测试只是自娱自乐
        assert!(
            HELP_MD.contains("```bash"),
            "help.md 里没有 bash 代码块,这条测试失去意义"
        );
        let md =
            "说明文字在这里。\n\n```bash\nenv -u WAYLAND_DISPLAY ./scrcpy-pad\n```\n\n后面的正文。";
        let ctx = egui::Context::default();
        let th = Theme::dark();
        let texts = run_md(&ctx, &th, md);

        // 阳性样本:代码块内容 → 等宽字体 + 有底色
        let code = texts
            .iter()
            .find(|(t, _, _, _)| t.contains("env -u WAYLAND_DISPLAY ./scrcpy-pad"))
            .expect("代码块内容没有被单独画出来(多半又被混进正文段落了)");
        assert_eq!(
            code.2,
            egui::FontFamily::Monospace,
            "代码块没有用等宽字体:它被当成正文排了"
        );
        assert_ne!(
            code.3,
            egui::Color32::TRANSPARENT,
            "代码块没有底色:和正文分不开"
        );

        // 阴性对照:同一帧里的普通正文必须是正文格式 —— 证明这条判据能区分两种样本,
        // 而不是"一律 Monospace"这种恒绿写法
        let para = texts
            .iter()
            .find(|(t, _, _, _)| t.contains("说明文字在这里"))
            .expect("正文段落没画出来");
        assert_eq!(para.2, egui::FontFamily::Proportional);
        assert_eq!(para.3, egui::Color32::TRANSPARENT);
    }
}
