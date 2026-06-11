# fish completion for ftool
# 使用 `ftool --help` 的输出内容生成补全

function __fish_ftool_using_subcommand
    set -l cmds (commandline -opc)
    set -e cmds[1]
    if test (count $cmds) -eq 0
        return 1
    end
    for i in (seq 1 (count $cmds))
        if test "$cmds[$i]" = "$argv[1]"
            return 0
        end
    end
    return 1
end

function __fish_ftool_has_subcommand
    set -l cmds (commandline -opc)
    set -e cmds[1]
    if test (count $cmds) -eq 0
        return 1
    end
    # 如果碰到任何子命令则返回 0
    for cmd in $cmds
        contains -- $cmd integrated compute hybrid nvidia query power switchable reset cache-create cache-delete cache-query
        and return 0
    end
    return 1
end

# 顶层参数补全 —— 不识别任何参数以外的内容
complete -c ftool -f

# 顶层命令
complete -c ftool -n "not __fish_seen_subcommand_from -S -U -g -H -h --help -V --version" \
    -s S -r -d "签名指定内核文件 (需要 root)"

complete -c ftool -n "not __fish_seen_subcommand_from -S -U -g -H -h --help -V --version" \
    -s U -d "系统版本升级 (需要 root)"

complete -c ftool -n "not __fish_seen_subcommand_from -S -U -g -H -h --help -V --version" \
    -s g -r -d "显卡模式切换与管理 (需要 root)"

complete -c ftool -n "not __fish_seen_subcommand_from -S -U -g -H -h --help -V --version" \
    -s H -r -k -d "计算文件哈希值"

complete -c ftool -n "not __fish_seen_subcommand_from -S -U -g -H -h --help -V --version" \
    -s h -l help -d "显示帮助"

complete -c ftool -n "not __fish_seen_subcommand_from -S -U -g -H -h --help -V --version" \
    -s V -l version -d "显示版本信息"

# ---- -g <操作> ----
complete -c ftool -n "__fish_seen_subcommand_from -g; and not __fish_ftool_has_subcommand" \
    -xa "integrated\t'仅使用集成显卡 (省电，屏蔽N卡)'
           compute\t'集显输出 + N卡计算 (省电+GPU计算)'
           hybrid\t'混合模式 (PRIME，按需渲染)'
           nvidia\t'仅使用 NVIDIA 显卡 (高性能)'
           query\t'查询当前显卡模式'
           power\t'运行时电源控制 (无需重启)'
           switchable\t'检测系统是否支持 GPU 切换'
           reset\t'还原 ftool 做出的所有修改'
           cache-create\t'创建显卡缓存'
           cache-delete\t'删除显卡缓存'
           cache-query\t'查询显卡缓存内容'"

# -g power 的子参数
complete -c ftool -n "__fish_seen_subcommand_from -g; and __fish_seen_subcommand_from power" \
    -xa "on\toff\tauto"

# -g integrated / compute / hybrid / nvidia 的高级选项
for __gpu_mode in integrated compute hybrid nvidia
    complete -c ftool -n "__fish_seen_subcommand_from -g; and __fish_seen_subcommand_from $__gpu_mode" \
        -l coolbits -r -d "启用 Coolbits (默认值: 28)"
    complete -c ftool -n "__fish_seen_subcommand_from -g; and __fish_seen_subcommand_from $__gpu_mode" \
        -l rtd3 -r -d "启用 RTD3 电源管理 (0-3, 默认: 2)"
    complete -c ftool -n "__fish_seen_subcommand_from -g; and __fish_seen_subcommand_from $__gpu_mode" \
        -l use-nvidia-current -d "使用 nvidia-current 内核模块"
end

# ---- -H <算法> <文件> ----
# 先补全算法
complete -c ftool -n "__fish_seen_subcommand_from -H; and not __fish_seen_subcommand_from md5 sha1 sha256 sha512" \
    -xa "md5\tsha1\tsha256\tsha512"

# 算法确定后补全文件路径
for __hash_algo in md5 sha1 sha256 sha512
    complete -c ftool -n "__fish_seen_subcommand_from -H; and __fish_seen_subcommand_from $__hash_algo" \
        -r -F -d "要计算哈希的文件"
end

# ---- -S <内核路径> ----
complete -c ftool -n "__fish_seen_subcommand_from -S" \
    -r -F -d "内核文件路径"

# -U 无额外参数
