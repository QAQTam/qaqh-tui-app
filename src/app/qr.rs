//! 终端里的二维码：两行模块压成一行半块字符。
//!
//! **为什么不是普通字符点阵**：终端一个字符单元是「高 ≈ 2×宽」的方块,一个模块占
//! 一整格会画出一个又瘦又高、手机很难对焦的码。用 `▀ ▄ █` 把**垂直相邻的两个模块**
//! 合并进一行,横竖方向的比例才回到 1:1。
//!
//! 只画黑、不写色：真彩/256 色终端里加深色背景更稳,但浅色主题下背景色反转就是废码
//! （本仓主题可切换）,而扫码只依赖前景/背景的**明暗对比**,半块字符本身已经给了。

use qrcode::{Color, EcLevel, QrCode};

/// 安静区（模块数）。规范值 4：扫码器靠它把码从背景里认出来,省掉会显著降低识别率。
const QUIET_ZONE: usize = 4;

/// 把文本编成二维码,渲染成终端行（每行一个字符串,宽度相同）。
///
/// `max_columns` 是可用终端列数;放不下就报错而不是画残缺的码——一个扫不上的码
/// 比一句错误提示更糟。
pub fn render_semiblocks(payload: &str, max_columns: usize) -> Result<Vec<String>, String> {
    // 用 L 档（不是 crate 默认的 M）：载荷是一条 120 秒就过期的配对令牌,不是长期
    // 凭证;L 让同样的内容窄掉 ~10 列,80 列终端里才留得下旁边的说明文字。
    let code = QrCode::with_error_correction_level(payload.as_bytes(), EcLevel::L)
        .map_err(|e| format!("二维码编码失败（载荷过长？）：{e}"))?;
    let width = code.width();
    let colors = code.to_colors();
    let total = width + QUIET_ZONE * 2;
    if total > max_columns {
        return Err(format!(
            "二维码宽 {total} 列，放不进 {max_columns} 列的终端（调宽窗口或缩短设备名）"
        ));
    }

    let is_dark = |x: usize, y: usize| -> bool {
        // 安静区与越界行都是「亮」。`total` 是奇数（码宽本身是奇数 + 8）,最后一行
        // 只有上半格有内容,下半格按亮处理。
        let (Some(cx), Some(cy)) = (x.checked_sub(QUIET_ZONE), y.checked_sub(QUIET_ZONE)) else {
            return false;
        };
        cx < width && cy < width && colors[cy * width + cx] != Color::Light
    };

    let rows = total.div_ceil(2);
    let mut lines = Vec::with_capacity(rows);
    for row in 0..rows {
        let (top, bottom) = (row * 2, row * 2 + 1);
        let mut line = String::with_capacity(total);
        for col in 0..total {
            line.push(match (is_dark(col, top), is_dark(col, bottom)) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            });
        }
        lines.push(line);
    }
    Ok(lines)
}

/// 载荷的模块宽度（含安静区）。**测试用**：断言「刚好放得下」的边界，
/// 生产路径由 `render_semiblocks` 自己判宽窄。
#[cfg(test)]
pub fn module_width(payload: &str) -> Option<usize> {
    QrCode::with_error_correction_level(payload.as_bytes(), EcLevel::L)
        .ok()
        .map(|code| code.width() + QUIET_ZONE * 2)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 半块渲染的基本形状：行数 = ceil(宽/2)、每行等宽、且**首尾行是安静区**
    /// （全亮 → 空格与 `▄`，绝不能出现 `█`——那意味着边框被画实,扫码器找不到边界）。
    #[test]
    fn renders_square_quiet_zone_layout() {
        let lines = render_semiblocks("http://192.168.1.8:64413/pair/abc", 200).expect("render");
        let total = lines[0].chars().count();
        assert_eq!(lines.len(), total.div_ceil(2), "两行模块并成一行");
        assert!(
            lines.iter().all(|line| line.chars().count() == total),
            "每行等宽"
        );
        assert!(
            !lines[0].contains('█') && !lines[0].contains('▀'),
            "最上面一行是安静区，不该有上半格的黑块"
        );
        // 码本身是正方形：模块数 == 行数×2。
        assert_eq!(total % 2, 1, "奇数模块宽（末行只有上半格）");
    }

    #[test]
    fn too_narrow_terminal_is_an_error_not_a_broken_code() {
        let payload = "http://192.168.1.8:64413/with/a/considerably/longer/pairing/payload";
        let needed = module_width(payload).expect("载荷可编码");
        let error = render_semiblocks(payload, needed - 1).expect_err("放不下必须报错");
        assert!(error.contains("放不进"), "{error}");
        // 刚好放得下就得成。
        assert!(render_semiblocks(payload, needed).is_ok());
    }

    /// 同一载荷必须得到**同一张图**（渲染进出缓存、以及测试可比对的前提）。
    #[test]
    fn rendering_is_deterministic() {
        let a = render_semiblocks("qaqh-pair", 200).expect("render a");
        let b = render_semiblocks("qaqh-pair", 200).expect("render b");
        assert_eq!(a, b);
    }
}
