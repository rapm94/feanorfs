const std = @import("std");

pub fn build(b: *std.Build) void {
    const target = b.standardTargetOptions(.{});
    const optimize = b.standardOptimizeOption(.{});

    const zig_agent = std.fs.path.dirname(@src().file) orelse ".";
    const ffi_inc = b.pathJoin(&.{ zig_agent, "..", "..", "feanorfs-ffi" });
    const profile_dir = switch (optimize) {
        .Debug => "debug",
        else => "release",
    };
    const lib_dir = b.pathJoin(&.{ zig_agent, "..", "..", "target", profile_dir });

    const exe = b.addExecutable(.{
        .name = "zig-agent-demo",
        .root_module = b.createModule(.{
            .root_source_file = b.path("main.zig"),
            .target = target,
            .optimize = optimize,
            .link_libc = true,
        }),
    });

    exe.root_module.addIncludePath(.{ .cwd_relative = ffi_inc });
    exe.root_module.addLibraryPath(.{ .cwd_relative = lib_dir });
    exe.root_module.addRPath(.{ .cwd_relative = lib_dir });
    exe.root_module.linkSystemLibrary("feanorfs_ffi", .{});
    // SQLite is bundled inside libfeanorfs_ffi (sqlx/libsqlite3-sys) — no -lsqlite3

    b.installArtifact(exe);
}
