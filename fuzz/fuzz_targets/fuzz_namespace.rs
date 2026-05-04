#![no_main]
use libfuzzer_sys::fuzz_target;
use ai_memory::models::*;

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        // Fuzz namespace_depth
        let _depth = namespace_depth(s);
        
        // Fuzz namespace_parent
        let _parent = namespace_parent(s);
        
        // Fuzz namespace_ancestors
        let _ancestors = namespace_ancestors(s);
        
        // Ensure round-trip invariants hold
        if !s.is_empty() {
            let depth = namespace_depth(s);
            let ancestors = namespace_ancestors(s);
            
            // Ancestors length should equal depth
            assert_eq!(ancestors.len(), depth,
                "Ancestors length {} != depth {} for namespace '{}'",
                ancestors.len(), depth, s);
            
            // First ancestor should be the namespace itself
            if !ancestors.is_empty() {
                assert_eq!(ancestors[0], s,
                    "First ancestor '{}' != namespace '{}'",
                    ancestors[0], s);
            }
            
            // Parent of self should be second ancestor (if exists)
            if ancestors.len() > 1 {
                assert_eq!(namespace_parent(s), Some(ancestors[1].clone()),
                    "Parent of '{}' != expected {}",
                    s, ancestors[1]);
            }
        }
    }
});
