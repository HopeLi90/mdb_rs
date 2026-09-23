//! 终端排版辅助：中文等全角字符在等宽字体里占两列，直接用 `{:<10}` 会算错宽度。

/// 单个字符的显示宽度（列数）
pub fn char_width(c: char) -> usize {
    if c.is_control() {
        return 0;
    }
    let u = c as u32;
    let wide = (0x1100..=0x115F).contains(&u)
        || (0x2E80..=0x303E).contains(&u)
        || (0x3041..=0x33FF).contains(&u)
        || (0x3400..=0x4DBF).contains(&u)
        || (0x4E00..=0x9FFF).contains(&u)
        || (0xA000..=0xA4CF).contains(&u)
        || (0xAC00..=0xD7A3).contains(&u)
        || (0xF900..=0xFAFF).contains(&u)
        || (0xFE30..=0xFE4F).contains(&u)
        || (0xFF00..=0xFF60).contains(&u)
        || (0xFFE0..=0xFFE6).contains(&u)
        || (0x1_F300..=0x1_F64F).contains(&u)
        || (0x2_0000..=0x3_FFFD).contains(&u);
    if wide {
        2
    } else {
        1
    }
}

/// 字符串的显示宽度（列数）
pub fn display_width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

/// 按显示宽度左对齐填充到指定列数
pub fn pad_display(s: &str, width: usize) -> String {
    let used = display_width(s);
    let mut out = s.to_string();
    if used < width {
        out.push_str(&" ".repeat(width - used));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{display_width, pad_display};

    #[test]
    fn counts_double_width_chars() {
        assert_eq!(display_width("abc"), 3);
        assert_eq!(display_width("要素类"), 6);
        assert_eq!(display_width("表"), 2);
        assert_eq!(display_width("Roads"), 5);
    }

    #[test]
    fn pads_to_column_width() {
        assert_eq!(display_width(&pad_display("表", 10)), 10);
        assert_eq!(display_width(&pad_display("要素数据集", 10)), 10);
        assert_eq!(pad_display("Roads", 6), "Roads ");
    }
}
