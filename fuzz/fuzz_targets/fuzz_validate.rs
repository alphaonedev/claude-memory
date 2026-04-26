#![no_main]
use libfuzzer_sys::fuzz_target;
use ai_memory::validate::*;

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        // Fuzz the validate_title function
        let _ = validate_title(s);
        
        // Fuzz the validate_namespace function
        let _ = validate_namespace(s);
        
        // Fuzz the validate_agent_id function
        let _ = validate_agent_id(s);
        
        // Fuzz the validate_scope function
        let _ = validate_scope(s);
        
        // Fuzz the validate_id function
        let _ = validate_id(s);
        
        // Fuzz the validate_source function
        let _ = validate_source(s);
    }
});
