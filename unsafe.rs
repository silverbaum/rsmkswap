


fn main() -> std::io::Result<()> {
        
    unsafe {
        let raw_ptr = alloc(layout);
        if raw_ptr.is_null() {
            std::alloc::handle_alloc_error(layout);
        }
        // Cast the raw pointer to a pointer to SwapHeader.
        let header_ptr = raw_ptr as *mut SwapHeader;

        // Now write directly into the allocated memory.
        // You can either use `ptr::write` or assign field-by-field.
        ptr::write(header_ptr, SwapHeader { magic: 0x12345678, size: 2048 });
        // Alternatively, if you want to modify fields one-by-one:
        (*header_ptr).magic = 0x12345678;
        (*header_ptr).size = 2048;
    }

}