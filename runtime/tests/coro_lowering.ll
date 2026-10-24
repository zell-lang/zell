; Reference lowering of Zell async/await onto LLVM switched-resume coroutines
; and the Zell runtime (ABI.md 10.3). Built and run by `make test` (needs clang).
;
; Source being modelled:
;
;   async func work(x int) int {
;       await task.Delay(5)          // suspends: the task is made here
;       return x * 2
;   }
;
;   async func outer(x int) int {
;       var a = await work(x)        // awaiting another async func
;       return a + 1
;   }
;
;   async func leaf(x int) int {     // no suspend point: no frame, no task
;       return x + 1
;   }
;
;   async func sum(x int) int {
;       var a = await leaf(x)        // completes synchronously: fast path
;       var b = await work(x)        // suspends
;       return a + b
;   }
;
;   func main() {                    // non-async main: blocking await
;       var t1 = outer(10)
;       var t2 = outer(20)
;       println(await t1)            // 21
;       println(await t2)            // 41
;       println(await sum(1))        // 4
;       println(await leaf(2))       // 3
;   }
;
; and, as the entry of an `async func main` (ABI.md 11), two ramps run by
; zell_task_run_main on a worker: one that suspends (sum(5) = 16, awaited
; with zell_task_await_main) and one that does not (leaf(7) = 8, read
; from res.value).
;
; Protocol: `void @f(params, ptr %res)` with %res -> { ptr task; T value }.
; The ramp stores the coroutine's Task<T> (+1) into res.task when the body
; suspends for the first time, or NULL plus the result (+1) into res.value
; when the body returns without ever suspending. The task itself is made by
; zell_task_lazy at the first suspension, not when the call starts.

declare ptr  @zell_task_lazy(ptr, i64, ptr, ptr)
declare ptr  @zell_task_completed(i64, ptr, ptr)
declare void @zell_coro_leave(ptr)
declare ptr  @zell_task_get_ctx()
declare void @zell_task_complete(ptr, ptr)
declare zeroext i1 @zell_task_is_done(ptr)
declare zeroext i1 @zell_task_add_continuation(ptr, ptr, ptr)
declare ptr  @zell_task_result(ptr)
declare ptr  @zell_task_await(ptr)
declare void @zell_task_run_main(ptr, ptr)
declare ptr  @zell_task_await_main(ptr)
declare ptr  @zell_task_delay(i64)
declare void @zell_coro_resume(ptr)
declare ptr  @zell_coro_frame_alloc(i64)
declare void @zell_coro_frame_free(ptr)
declare void @zell_retain(ptr)
declare void @zell_release(ptr)
declare void @zell_println_int(i64)
declare void @zell_runtime_init(i32, ptr)
declare void @zell_runtime_shutdown()

declare token @llvm.coro.id(i32, ptr, ptr, ptr)
declare i1    @llvm.coro.alloc(token)
declare i64   @llvm.coro.size.i64()
declare ptr   @llvm.coro.begin(token, ptr)
declare token @llvm.coro.save(ptr)
declare i8    @llvm.coro.suspend(token, i1)
declare ptr   @llvm.coro.free(token, ptr)
declare i1    @llvm.coro.end(ptr, i1, token)

%res.i64 = type { ptr, i64 }     ; { Task<int> task; int value }

; ---------------------------------------------------------------- work(x)
define void @work(i64 %x, ptr %res) presplitcoroutine {
entry:
  %ret.slot = alloca i64, align 8
  %task.slot = alloca ptr, align 8      ; the coroutine's task: NULL until its first suspension
  %ctx.slot = alloca ptr, align 8       ; the caller's context, +1 (zell_task_get_ctx)
  ; 1. frame on the zell heap, unless LLVM proves the coroutine never suspends
  %id   = call token @llvm.coro.id(i32 0, ptr null, ptr null, ptr null)
  %need = call i1 @llvm.coro.alloc(token %id)
  br i1 %need, label %alloc, label %begin

alloc:
  %size = call i64 @llvm.coro.size.i64()
  %mem  = call ptr @zell_coro_frame_alloc(i64 %size)
  br label %begin

begin:
  %memphi = phi ptr [ null, %entry ], [ %mem, %alloc ]
  %hdl  = call ptr @llvm.coro.begin(token %id, ptr %memphi)
  store ptr null, ptr %task.slot
  %ctx0 = call ptr @zell_task_get_ctx()
  store ptr %ctx0, ptr %ctx.slot
  ; 2. body: `await task.Delay(5)` (result type void)
  %d = call ptr @zell_task_delay(i64 5)
  %d.done = call i1 @zell_task_is_done(ptr %d)
  br i1 %d.done, label %d.ready, label %d.try

d.try:
  ; the coroutine's own task (made now if this is its first suspension)
  %d.cur = load ptr, ptr %task.slot
  %d.ctx = load ptr, ptr %ctx.slot
  %d.own = call ptr @zell_task_lazy(ptr %d.cur, i64 8, ptr null, ptr %d.ctx)
  store ptr %d.own, ptr %task.slot
  %d.first = icmp eq ptr %d.cur, null
  br i1 %d.first, label %d.publish, label %d.save

d.publish:                                  ; first suspension: hand the task to the caller
  %d.rt = getelementptr inbounds %res.i64, ptr %res, i64 0, i32 0
  store ptr %d.own, ptr %d.rt
  br label %d.save

d.save:
  %d.sv  = call token @llvm.coro.save(ptr %hdl)
  %d.reg = call i1 @zell_task_add_continuation(ptr %d, ptr @zell_coro_resume, ptr %hdl)
  br i1 %d.reg, label %d.suspend, label %d.ready     ; false: completed meanwhile

d.suspend:
  %d.s = call i8 @llvm.coro.suspend(token %d.sv, i1 false)
  switch i8 %d.s, label %done [i8 0, label %d.ready
                               i8 1, label %cleanup]

d.ready:
  call void @zell_release(ptr %d)
  ; 3. `return x * 2`
  %r = mul i64 %x, 2
  store i64 %r, ptr %ret.slot
  %own = load ptr, ptr %task.slot
  %sync = icmp eq ptr %own, null
  br i1 %sync, label %fin.sync, label %fin.task

fin.sync:                                   ; never suspended: result and NULL task to the caller
  %ctx1 = load ptr, ptr %ctx.slot
  call void @zell_coro_leave(ptr %ctx1)
  %s.rt = getelementptr inbounds %res.i64, ptr %res, i64 0, i32 0
  store ptr null, ptr %s.rt
  %s.rv = getelementptr inbounds %res.i64, ptr %res, i64 0, i32 1
  %v = load i64, ptr %ret.slot
  store i64 %v, ptr %s.rv
  br label %cleanup

fin.task:                                   ; complete the task, drop the frame's reference
  call void @zell_task_complete(ptr %own, ptr %ret.slot)
  call void @zell_release(ptr %own)
  br label %cleanup

cleanup:                                    ; no final suspend: the frame frees itself
  %mem.free = call ptr @llvm.coro.free(token %id, ptr %hdl)
  %has.mem = icmp ne ptr %mem.free, null
  br i1 %has.mem, label %free, label %done

free:
  call void @zell_coro_frame_free(ptr %mem.free)
  br label %done

done:
  %end = call i1 @llvm.coro.end(ptr %hdl, i1 false, token none)
  ret void
}

; ---------------------------------------------------------------- outer(x)
define void @outer(i64 %x, ptr %res) presplitcoroutine {
entry:
  %ret.slot = alloca i64, align 8
  %task.slot = alloca ptr, align 8
  %ctx.slot = alloca ptr, align 8
  %a.res = alloca %res.i64, align 8       ; result slot of the awaited call
  %id   = call token @llvm.coro.id(i32 0, ptr null, ptr null, ptr null)
  %need = call i1 @llvm.coro.alloc(token %id)
  br i1 %need, label %alloc, label %begin

alloc:
  %size = call i64 @llvm.coro.size.i64()
  %mem  = call ptr @zell_coro_frame_alloc(i64 %size)
  br label %begin

begin:
  %memphi = phi ptr [ null, %entry ], [ %mem, %alloc ]
  %hdl  = call ptr @llvm.coro.begin(token %id, ptr %memphi)
  store ptr null, ptr %task.slot
  %ctx0 = call ptr @zell_task_get_ctx()
  store ptr %ctx0, ptr %ctx.slot
  ; `var a = await work(x)`: a NULL task means work completed synchronously
  call void @work(i64 %x, ptr %a.res)
  %w.tp = getelementptr inbounds %res.i64, ptr %a.res, i64 0, i32 0
  %w = load ptr, ptr %w.tp
  %w.sync = icmp eq ptr %w, null
  br i1 %w.sync, label %w.fast, label %w.check

w.fast:
  %w.vp = getelementptr inbounds %res.i64, ptr %a.res, i64 0, i32 1
  %a.f = load i64, ptr %w.vp
  br label %w.join

w.check:
  %w.done = call i1 @zell_task_is_done(ptr %w)
  br i1 %w.done, label %w.ready, label %w.try

w.try:
  %w.cur = load ptr, ptr %task.slot
  %w.ctx = load ptr, ptr %ctx.slot
  %w.own = call ptr @zell_task_lazy(ptr %w.cur, i64 8, ptr null, ptr %w.ctx)
  store ptr %w.own, ptr %task.slot
  %w.first = icmp eq ptr %w.cur, null
  br i1 %w.first, label %w.publish, label %w.save

w.publish:
  %w.rt = getelementptr inbounds %res.i64, ptr %res, i64 0, i32 0
  store ptr %w.own, ptr %w.rt
  br label %w.save

w.save:
  %w.sv  = call token @llvm.coro.save(ptr %hdl)
  %w.reg = call i1 @zell_task_add_continuation(ptr %w, ptr @zell_coro_resume, ptr %hdl)
  br i1 %w.reg, label %w.suspend, label %w.ready

w.suspend:
  %w.s = call i8 @llvm.coro.suspend(token %w.sv, i1 false)
  switch i8 %w.s, label %done [i8 0, label %w.ready
                               i8 1, label %cleanup]

w.ready:
  %w.slot = call ptr @zell_task_result(ptr %w)   ; borrowed slot
  %a.s = load i64, ptr %w.slot                    ; (retain here if T is refcounted)
  call void @zell_release(ptr %w)
  br label %w.join

w.join:
  %a = phi i64 [ %a.f, %w.fast ], [ %a.s, %w.ready ]
  %r = add i64 %a, 1
  store i64 %r, ptr %ret.slot
  %own = load ptr, ptr %task.slot
  %sync = icmp eq ptr %own, null
  br i1 %sync, label %fin.sync, label %fin.task

fin.sync:
  %ctx1 = load ptr, ptr %ctx.slot
  call void @zell_coro_leave(ptr %ctx1)
  %s.rt = getelementptr inbounds %res.i64, ptr %res, i64 0, i32 0
  store ptr null, ptr %s.rt
  %s.rv = getelementptr inbounds %res.i64, ptr %res, i64 0, i32 1
  %v = load i64, ptr %ret.slot
  store i64 %v, ptr %s.rv
  br label %cleanup

fin.task:
  call void @zell_task_complete(ptr %own, ptr %ret.slot)
  call void @zell_release(ptr %own)
  br label %cleanup

cleanup:
  %mem.free = call ptr @llvm.coro.free(token %id, ptr %hdl)
  %has.mem = icmp ne ptr %mem.free, null
  br i1 %has.mem, label %free, label %done

free:
  call void @zell_coro_frame_free(ptr %mem.free)
  br label %done

done:
  %end = call i1 @llvm.coro.end(ptr %hdl, i1 false, token none)
  ret void
}

; ---------------------------------------------------------------- leaf(x)
; No suspend point: coro.alloc folds to false and coro.free to null, so the
; frame is never allocated and the function is a plain call.
define void @leaf(i64 %x, ptr %res) presplitcoroutine {
entry:
  %ret.slot = alloca i64, align 8
  %task.slot = alloca ptr, align 8
  %ctx.slot = alloca ptr, align 8
  %id   = call token @llvm.coro.id(i32 0, ptr null, ptr null, ptr null)
  %need = call i1 @llvm.coro.alloc(token %id)
  br i1 %need, label %alloc, label %begin

alloc:
  %size = call i64 @llvm.coro.size.i64()
  %mem  = call ptr @zell_coro_frame_alloc(i64 %size)
  br label %begin

begin:
  %memphi = phi ptr [ null, %entry ], [ %mem, %alloc ]
  %hdl  = call ptr @llvm.coro.begin(token %id, ptr %memphi)
  store ptr null, ptr %task.slot
  %ctx0 = call ptr @zell_task_get_ctx()
  store ptr %ctx0, ptr %ctx.slot
  %r = add i64 %x, 1
  store i64 %r, ptr %ret.slot
  %own = load ptr, ptr %task.slot
  %sync = icmp eq ptr %own, null
  br i1 %sync, label %fin.sync, label %fin.task

fin.sync:
  %ctx1 = load ptr, ptr %ctx.slot
  call void @zell_coro_leave(ptr %ctx1)
  %s.rt = getelementptr inbounds %res.i64, ptr %res, i64 0, i32 0
  store ptr null, ptr %s.rt
  %s.rv = getelementptr inbounds %res.i64, ptr %res, i64 0, i32 1
  %v = load i64, ptr %ret.slot
  store i64 %v, ptr %s.rv
  br label %cleanup

fin.task:
  call void @zell_task_complete(ptr %own, ptr %ret.slot)
  call void @zell_release(ptr %own)
  br label %cleanup

cleanup:
  %mem.free = call ptr @llvm.coro.free(token %id, ptr %hdl)
  %has.mem = icmp ne ptr %mem.free, null
  br i1 %has.mem, label %free, label %done

free:
  call void @zell_coro_frame_free(ptr %mem.free)
  br label %done

done:
  %end = call i1 @llvm.coro.end(ptr %hdl, i1 false, token none)
  ret void
}

; ---------------------------------------------------------------- sum(x)
define void @sum(i64 %x, ptr %res) presplitcoroutine {
entry:
  %ret.slot = alloca i64, align 8
  %task.slot = alloca ptr, align 8
  %ctx.slot = alloca ptr, align 8
  %a.res = alloca %res.i64, align 8
  %b.res = alloca %res.i64, align 8
  %id   = call token @llvm.coro.id(i32 0, ptr null, ptr null, ptr null)
  %need = call i1 @llvm.coro.alloc(token %id)
  br i1 %need, label %alloc, label %begin

alloc:
  %size = call i64 @llvm.coro.size.i64()
  %mem  = call ptr @zell_coro_frame_alloc(i64 %size)
  br label %begin

begin:
  %memphi = phi ptr [ null, %entry ], [ %mem, %alloc ]
  %hdl  = call ptr @llvm.coro.begin(token %id, ptr %memphi)
  store ptr null, ptr %task.slot
  %ctx0 = call ptr @zell_task_get_ctx()
  store ptr %ctx0, ptr %ctx.slot
  ; `var a = await leaf(x)`: always the fast path
  call void @leaf(i64 %x, ptr %a.res)
  %l.tp = getelementptr inbounds %res.i64, ptr %a.res, i64 0, i32 0
  %l = load ptr, ptr %l.tp
  %l.sync = icmp eq ptr %l, null
  br i1 %l.sync, label %l.fast, label %l.check

l.fast:
  %l.vp = getelementptr inbounds %res.i64, ptr %a.res, i64 0, i32 1
  %a.f = load i64, ptr %l.vp
  br label %l.join

l.check:
  %l.done = call i1 @zell_task_is_done(ptr %l)
  br i1 %l.done, label %l.ready, label %l.try

l.try:
  %l.cur = load ptr, ptr %task.slot
  %l.ctx = load ptr, ptr %ctx.slot
  %l.own = call ptr @zell_task_lazy(ptr %l.cur, i64 8, ptr null, ptr %l.ctx)
  store ptr %l.own, ptr %task.slot
  %l.first = icmp eq ptr %l.cur, null
  br i1 %l.first, label %l.publish, label %l.save

l.publish:
  %l.rt = getelementptr inbounds %res.i64, ptr %res, i64 0, i32 0
  store ptr %l.own, ptr %l.rt
  br label %l.save

l.save:
  %l.sv  = call token @llvm.coro.save(ptr %hdl)
  %l.reg = call i1 @zell_task_add_continuation(ptr %l, ptr @zell_coro_resume, ptr %hdl)
  br i1 %l.reg, label %l.suspend, label %l.ready

l.suspend:
  %l.s = call i8 @llvm.coro.suspend(token %l.sv, i1 false)
  switch i8 %l.s, label %done [i8 0, label %l.ready
                               i8 1, label %cleanup]

l.ready:
  %l.slot = call ptr @zell_task_result(ptr %l)
  %a.s = load i64, ptr %l.slot
  call void @zell_release(ptr %l)
  br label %l.join

l.join:
  %a = phi i64 [ %a.f, %l.fast ], [ %a.s, %l.ready ]
  ; `var b = await work(x)`: suspends
  call void @work(i64 %x, ptr %b.res)
  %w.tp = getelementptr inbounds %res.i64, ptr %b.res, i64 0, i32 0
  %w = load ptr, ptr %w.tp
  %w.sync = icmp eq ptr %w, null
  br i1 %w.sync, label %w.fast, label %w.check

w.fast:
  %w.vp = getelementptr inbounds %res.i64, ptr %b.res, i64 0, i32 1
  %b.f = load i64, ptr %w.vp
  br label %w.join

w.check:
  %w.done = call i1 @zell_task_is_done(ptr %w)
  br i1 %w.done, label %w.ready, label %w.try

w.try:
  %w.cur = load ptr, ptr %task.slot
  %w.ctx = load ptr, ptr %ctx.slot
  %w.own = call ptr @zell_task_lazy(ptr %w.cur, i64 8, ptr null, ptr %w.ctx)
  store ptr %w.own, ptr %task.slot
  %w.first = icmp eq ptr %w.cur, null
  br i1 %w.first, label %w.publish, label %w.save

w.publish:
  %w.rt = getelementptr inbounds %res.i64, ptr %res, i64 0, i32 0
  store ptr %w.own, ptr %w.rt
  br label %w.save

w.save:
  %w.sv  = call token @llvm.coro.save(ptr %hdl)
  %w.reg = call i1 @zell_task_add_continuation(ptr %w, ptr @zell_coro_resume, ptr %hdl)
  br i1 %w.reg, label %w.suspend, label %w.ready

w.suspend:
  %w.s = call i8 @llvm.coro.suspend(token %w.sv, i1 false)
  switch i8 %w.s, label %done [i8 0, label %w.ready
                               i8 1, label %cleanup]

w.ready:
  %w.slot = call ptr @zell_task_result(ptr %w)
  %b.s = load i64, ptr %w.slot
  call void @zell_release(ptr %w)
  br label %w.join

w.join:
  %b = phi i64 [ %b.f, %w.fast ], [ %b.s, %w.ready ]
  %r = add i64 %a, %b
  store i64 %r, ptr %ret.slot
  %own = load ptr, ptr %task.slot
  %sync = icmp eq ptr %own, null
  br i1 %sync, label %fin.sync, label %fin.task

fin.sync:
  %ctx1 = load ptr, ptr %ctx.slot
  call void @zell_coro_leave(ptr %ctx1)
  %s.rt = getelementptr inbounds %res.i64, ptr %res, i64 0, i32 0
  store ptr null, ptr %s.rt
  %s.rv = getelementptr inbounds %res.i64, ptr %res, i64 0, i32 1
  %v = load i64, ptr %ret.slot
  store i64 %v, ptr %s.rv
  br label %cleanup

fin.task:
  call void @zell_task_complete(ptr %own, ptr %ret.slot)
  call void @zell_release(ptr %own)
  br label %cleanup

cleanup:
  %mem.free = call ptr @llvm.coro.free(token %id, ptr %hdl)
  %has.mem = icmp ne ptr %mem.free, null
  br i1 %has.mem, label %free, label %done

free:
  call void @zell_coro_frame_free(ptr %mem.free)
  br label %done

done:
  %end = call i1 @llvm.coro.end(ptr %hdl, i1 false, token none)
  ret void
}

; ---------------------------------------------------------------- main
define i32 @main(i32 %argc, ptr %argv) {
entry:
  %r1 = alloca %res.i64, align 8
  %r2 = alloca %res.i64, align 8
  %r3 = alloca %res.i64, align 8
  %r4 = alloca %res.i64, align 8
  call void @zell_runtime_init(i32 %argc, ptr %argv)
  ; `var t1 = outer(10)`: a kept task is materialised when the call completed synchronously
  call void @outer(i64 10, ptr %r1)
  %t1.tp = getelementptr inbounds %res.i64, ptr %r1, i64 0, i32 0
  %t1.r = load ptr, ptr %t1.tp
  %t1.n = icmp eq ptr %t1.r, null
  br i1 %t1.n, label %t1.mat, label %t1.ok

t1.mat:
  %t1.vp = getelementptr inbounds %res.i64, ptr %r1, i64 0, i32 1
  %t1.c = call ptr @zell_task_completed(i64 8, ptr null, ptr %t1.vp)
  br label %t1.ok

t1.ok:
  %t1 = phi ptr [ %t1.r, %entry ], [ %t1.c, %t1.mat ]
  call void @outer(i64 20, ptr %r2)
  %t2.tp = getelementptr inbounds %res.i64, ptr %r2, i64 0, i32 0
  %t2.r = load ptr, ptr %t2.tp
  %t2.n = icmp eq ptr %t2.r, null
  br i1 %t2.n, label %t2.mat, label %t2.ok

t2.mat:
  %t2.vp = getelementptr inbounds %res.i64, ptr %r2, i64 0, i32 1
  %t2.c = call ptr @zell_task_completed(i64 8, ptr null, ptr %t2.vp)
  br label %t2.ok

t2.ok:
  %t2 = phi ptr [ %t2.r, %t1.ok ], [ %t2.c, %t2.mat ]
  %p1 = call ptr @zell_task_await(ptr %t1)      ; blocks the main thread
  %v1 = load i64, ptr %p1
  call void @zell_println_int(i64 %v1)
  %p2 = call ptr @zell_task_await(ptr %t2)
  %v2 = load i64, ptr %p2
  call void @zell_println_int(i64 %v2)
  call void @zell_release(ptr %t1)
  call void @zell_release(ptr %t2)
  ; `println(await sum(1))` and `println(await leaf(2))` in non-async code
  call void @sum(i64 1, ptr %r3)
  %s.tp = getelementptr inbounds %res.i64, ptr %r3, i64 0, i32 0
  %s.t = load ptr, ptr %s.tp
  %s.n = icmp eq ptr %s.t, null
  br i1 %s.n, label %s.fast, label %s.slow

s.fast:
  %s.vp = getelementptr inbounds %res.i64, ptr %r3, i64 0, i32 1
  %s.f = load i64, ptr %s.vp
  br label %s.join

s.slow:
  %s.p = call ptr @zell_task_await(ptr %s.t)
  %s.s = load i64, ptr %s.p
  call void @zell_release(ptr %s.t)
  br label %s.join

s.join:
  %s.v = phi i64 [ %s.f, %s.fast ], [ %s.s, %s.slow ]
  call void @zell_println_int(i64 %s.v)
  call void @leaf(i64 2, ptr %r4)
  %l.tp = getelementptr inbounds %res.i64, ptr %r4, i64 0, i32 0
  %l.t = load ptr, ptr %l.tp
  %l.n = icmp eq ptr %l.t, null
  br i1 %l.n, label %l.fast, label %l.slow

l.fast:
  %l.vp = getelementptr inbounds %res.i64, ptr %r4, i64 0, i32 1
  %l.f = load i64, ptr %l.vp
  br label %l.join

l.slow:
  %l.p = call ptr @zell_task_await(ptr %l.t)
  %l.s = load i64, ptr %l.p
  call void @zell_release(ptr %l.t)
  br label %l.join

l.join:
  %l.v = phi i64 [ %l.f, %l.fast ], [ %l.s, %l.slow ]
  call void @zell_println_int(i64 %l.v)
  %m1 = call i64 @async_main(ptr @main_ramp_sum)
  call void @zell_println_int(i64 %m1)
  %m2 = call i64 @async_main(ptr @main_ramp_leaf)
  call void @zell_println_int(i64 %m2)
  call void @zell_runtime_shutdown()
  ret i32 0
}

; The ramps of two modelled `async func main`s: `void(ptr %res)`.
define void @main_ramp_sum(ptr %res) {
  call void @sum(i64 5, ptr %res)
  ret void
}

define void @main_ramp_leaf(ptr %res) {
  call void @leaf(i64 7, ptr %res)
  ret void
}

; What the compiler emits in C `main` for an `async func main` (ABI.md 11).
define i64 @async_main(ptr %ramp) {
entry:
  %res = alloca %res.i64, align 8
  call void @zell_task_run_main(ptr %ramp, ptr %res)  ; ramp runs on a worker
  %tp = getelementptr inbounds %res.i64, ptr %res, i64 0, i32 0
  %t = load ptr, ptr %tp
  %n = icmp eq ptr %t, null
  br i1 %n, label %fast, label %slow

fast:
  %vp = getelementptr inbounds %res.i64, ptr %res, i64 0, i32 1
  %f = load i64, ptr %vp
  br label %join

slow:
  %p = call ptr @zell_task_await_main(ptr %t)
  %s = load i64, ptr %p
  call void @zell_release(ptr %t)
  br label %join

join:
  %v = phi i64 [ %f, %fast ], [ %s, %slow ]
  ret i64 %v
}
