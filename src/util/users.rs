//! User names by uid from /etc/passwd (users from network directories show
//! as their uid).

use std::collections::HashMap;

pub fn passwd_names() -> HashMap<u32, String> {
    let mut users = HashMap::new();
    if let Ok(s) = std::fs::read_to_string("/etc/passwd") {
        for l in s.lines() {
            let f: Vec<&str> = l.split(':').collect();
            if f.len() > 2
                && let Ok(uid) = f[2].parse::<u32>()
            {
                users.insert(uid, f[0].to_string());
            }
        }
    }
    users
}
