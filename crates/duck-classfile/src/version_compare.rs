pub fn compare(v1: &str, v2: &str) -> i32 {
    if v1 == "-1" && v2 == "-1" {
        return 0;
    }
    if v1 == "-1" {
        return -1;
    }
    if v2 == "-1" {
        return 1;
    }

    let parts1: Vec<&str> = v1.split('.').collect();
    let parts2: Vec<&str> = v2.split('.').collect();
    let max_len = parts1.len().max(parts2.len());

    for i in 0..max_len {
        let n1 = parts1
            .get(i)
            .and_then(|s| s.parse::<i32>().ok())
            .unwrap_or(0);
        let n2 = parts2
            .get(i)
            .and_then(|s| s.parse::<i32>().ok())
            .unwrap_or(0);
        if n1 > n2 {
            return 1;
        }
        if n1 < n2 {
            return -1;
        }
    }
    0
}

pub fn diff_num(v1: &str, v2: &str) -> i32 {
    let parts1: Vec<i32> = v1.split('.').filter_map(|s| s.parse().ok()).collect();
    let parts2: Vec<i32> = v2.split('.').filter_map(|s| s.parse().ok()).collect();
    let max_len = parts1.len().max(parts2.len());
    let mut diff = 0;
    for i in 0..max_len {
        let n1 = parts1.get(i).copied().unwrap_or(0);
        let n2 = parts2.get(i).copied().unwrap_or(0);
        diff += (n1 - n2).abs();
    }
    diff
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compare_equal() {
        assert_eq!(compare("3.0.5", "3.0.5"), 0);
        assert_eq!(compare("3.0.0.5", "3.0.0.5"), 0);
    }

    #[test]
    fn test_compare_greater() {
        assert_eq!(compare("3.0.6", "3.0.5"), 1);
        assert_eq!(compare("3.1.0", "3.0.9"), 1);
    }

    #[test]
    fn test_compare_less() {
        assert_eq!(compare("3.0.5", "3.0.6"), -1);
        assert_eq!(compare("3.0.9", "3.1.0"), -1);
    }

    #[test]
    fn test_compare_diff_len() {
        assert_eq!(compare("3.0.5", "3.0.5.1"), -1);
        assert_eq!(compare("3.0.5.1", "3.0.5"), 1);
    }

    #[test]
    fn test_compare_invalid() {
        assert_eq!(compare("-1", "3.0.5"), -1);
        assert_eq!(compare("3.0.5", "-1"), 1);
        assert_eq!(compare("-1", "-1"), 0);
    }

    #[test]
    fn test_diff_num() {
        assert_eq!(diff_num("3.0.5", "3.0.6"), 1);
        assert_eq!(diff_num("3.0.5", "4.0.5"), 1);
        assert_eq!(diff_num("3.0.5", "3.0.5"), 0);
    }
}
