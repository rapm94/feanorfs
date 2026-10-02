const std = @import("std");
const c = @cImport({
    @cInclude("feanorfs.h");
    @cInclude("stdio.h");
});

pub fn main() !void {
    if (c.ffs_runtime_init() != 0) {
        const err = c.ffs_last_error();
        defer c.ffs_string_free(err);
        std.debug.print("runtime init failed: {s}\n", .{std.mem.span(err)});
        return error.RuntimeInit;
    }

    const root_env = std.c.getenv("FEANORFS_WORKSPACE") orelse {
        std.debug.print("Set FEANORFS_WORKSPACE to a configured workspace path\n", .{});
        return error.NoWorkspace;
    };
    const root = std.mem.span(root_env);

    const name = "zig1";
    const spawn = c.ffs_agent_spawn(root.ptr, name.ptr, 0, 0);
    if (spawn == null) {
        const err = c.ffs_last_error();
        defer c.ffs_string_free(err);
        std.debug.print("spawn failed: {s}\n", .{std.mem.span(err)});
        return error.SpawnFailed;
    }
    defer c.ffs_string_free(spawn);
    std.debug.print("spawn: {s}\n", .{std.mem.span(spawn)});

    const agent_dir = c.ffs_agent_path(root.ptr, name.ptr);
    if (agent_dir == null) {
        const err = c.ffs_last_error();
        defer c.ffs_string_free(err);
        std.debug.print("agent path failed: {s}\n", .{std.mem.span(err)});
        return error.AgentPathFailed;
    }
    defer c.ffs_string_free(agent_dir);

    const agent_task = try std.fmt.allocPrintSentinel(
        std.heap.page_allocator,
        "{s}/task.txt",
        .{std.mem.span(agent_dir)},
        0,
    );
    defer std.heap.page_allocator.free(agent_task);
    const fp = c.fopen(agent_task.ptr, "w") orelse return error.WriteFailed;
    const write_failed = c.fputs("zig edit\n", fp) == c.EOF;
    // fclose flushes buffered output. Always close, even after a write error,
    // and never publish the agent workspace unless both operations succeeded.
    const close_failed = c.fclose(fp) == c.EOF;
    if (write_failed or close_failed) return error.WriteFailed;

    const land = c.ffs_agent_land(root.ptr, name.ptr, 0, 0);
    if (land == null) {
        const err = c.ffs_last_error();
        defer c.ffs_string_free(err);
        std.debug.print("land failed: {s}\n", .{std.mem.span(err)});
        return error.LandFailed;
    }
    defer c.ffs_string_free(land);
    std.debug.print("land: {s}\n", .{std.mem.span(land)});

    const clean = c.ffs_agent_clean(root.ptr, name.ptr);
    if (clean == null) {
        const err = c.ffs_last_error();
        defer c.ffs_string_free(err);
        std.debug.print("clean failed: {s}\n", .{std.mem.span(err)});
        return error.CleanFailed;
    }
    defer c.ffs_string_free(clean);
    std.debug.print("clean: {s}\n", .{std.mem.span(clean)});
}
