(module
    (global $bump (mut i32) (i32.const 65536))

    (func (export "alloc") (param $size i32) (result i32)
        (local $ptr i32)
        ;; Reset bump to heap base — by the time host calls alloc again,
        ;; the previous event and output have already been read.
        (global.set $bump (i32.const 65536))
        (local.set $ptr (global.get $bump))
        (global.set $bump (i32.add (global.get $bump) (local.get $size)))
        (local.get $ptr)
    )

    (func (export "dealloc") (param $ptr i32) (param $size i32))

    (func (export "process") (param $ptr i32) (param $len i32) (result i32)
        (local $pos i32)
        (local $src_len i32)
        (local $meta_count i32)
        (local $meta_i i32)
        (local $skip_len i32)
        (local $payload_len i32)
        (local $payload_ptr i32)
        (local $out_ptr i32)
        (local $write_pos i32)
        (local $result_content_len i32)

        ;; Reset bump allocator — host reads output immediately after return,
        ;; so previous allocations are no longer needed. Place output after
        ;; the input event to avoid overwriting it during this call.
        (global.set $bump (i32.add (local.get $ptr) (local.get $len)))

        ;; Skip UUID (16) + timestamp (8) = 24 bytes
        (local.set $pos (i32.add (local.get $ptr) (i32.const 24)))

        ;; Skip source string
        (local.set $src_len (i32.load (local.get $pos)))
        (local.set $pos (i32.add (local.get $pos) (i32.add (i32.const 4) (local.get $src_len))))

        ;; Skip partition (2 bytes)
        (local.set $pos (i32.add (local.get $pos) (i32.const 2)))

        ;; Skip metadata entries
        (local.set $meta_count (i32.load (local.get $pos)))
        (local.set $pos (i32.add (local.get $pos) (i32.const 4)))
        (local.set $meta_i (i32.const 0))
        (block $break_meta
            (loop $loop_meta
                (br_if $break_meta (i32.ge_u (local.get $meta_i) (local.get $meta_count)))
                (local.set $skip_len (i32.load (local.get $pos)))
                (local.set $pos (i32.add (local.get $pos) (i32.add (i32.const 4) (local.get $skip_len))))
                (local.set $skip_len (i32.load (local.get $pos)))
                (local.set $pos (i32.add (local.get $pos) (i32.add (i32.const 4) (local.get $skip_len))))
                (local.set $meta_i (i32.add (local.get $meta_i) (i32.const 1)))
                (br $loop_meta)
            )
        )

        ;; Read payload
        (local.set $payload_len (i32.load (local.get $pos)))
        (local.set $payload_ptr (i32.add (local.get $pos) (i32.const 4)))

        ;; Output: 4(count) + 4(dest_len) + 6("output") + 1(no_key) + 4(payload_len) + payload + 4(headers=0)
        (local.set $result_content_len (i32.add (i32.const 23) (local.get $payload_len)))

        ;; Allocate and write output
        (local.set $out_ptr (call $bump_alloc (i32.add (i32.const 4) (local.get $result_content_len))))
        (local.set $write_pos (local.get $out_ptr))

        ;; Length prefix
        (i32.store (local.get $write_pos) (local.get $result_content_len))
        (local.set $write_pos (i32.add (local.get $write_pos) (i32.const 4)))

        ;; Output count = 1
        (i32.store (local.get $write_pos) (i32.const 1))
        (local.set $write_pos (i32.add (local.get $write_pos) (i32.const 4)))

        ;; Destination = "output" (6 bytes)
        (i32.store (local.get $write_pos) (i32.const 6))
        (local.set $write_pos (i32.add (local.get $write_pos) (i32.const 4)))
        (i32.store8 (local.get $write_pos) (i32.const 111))
        (i32.store8 (i32.add (local.get $write_pos) (i32.const 1)) (i32.const 117))
        (i32.store8 (i32.add (local.get $write_pos) (i32.const 2)) (i32.const 116))
        (i32.store8 (i32.add (local.get $write_pos) (i32.const 3)) (i32.const 112))
        (i32.store8 (i32.add (local.get $write_pos) (i32.const 4)) (i32.const 117))
        (i32.store8 (i32.add (local.get $write_pos) (i32.const 5)) (i32.const 116))
        (local.set $write_pos (i32.add (local.get $write_pos) (i32.const 6)))

        ;; No key
        (i32.store8 (local.get $write_pos) (i32.const 0))
        (local.set $write_pos (i32.add (local.get $write_pos) (i32.const 1)))

        ;; Payload
        (i32.store (local.get $write_pos) (local.get $payload_len))
        (local.set $write_pos (i32.add (local.get $write_pos) (i32.const 4)))
        (memory.copy (local.get $write_pos) (local.get $payload_ptr) (local.get $payload_len))
        (local.set $write_pos (i32.add (local.get $write_pos) (local.get $payload_len)))

        ;; Headers = 0
        (i32.store (local.get $write_pos) (i32.const 0))

        (local.get $out_ptr)
    )

    (func $bump_alloc (param $size i32) (result i32)
        (local $ptr i32)
        (local.set $ptr (global.get $bump))
        (global.set $bump (i32.add (global.get $bump) (local.get $size)))
        (local.get $ptr)
    )

    (memory (export "memory") 4)
)
