# bash completion for ftool
# 根据 `ftool --help` 的输出内容生成补全

_ftool() {
    local cur prev words cword
    _init_completion -s || return

    # 当前已经解析到的参数
    local -a args=("${words[@]:1:$cword-1}")

    # 检测顶层参数是否已经出现
    local has_S= has_U= has_g= has_H= has_h= has_V=
    local has_subcmd=
    local has_hash_algo=
    local i
    for ((i = 0; i < ${#words[@]} - 1; i++)); do
        case "${words[i]}" in
            -S) has_S=1 ;;
            -U) has_U=1 ;;
            -g) has_g=1 ;;
            -H) has_H=1 ;;
            -h|--help) has_h=1 ;;
            -V|--version) has_V=1 ;;
            integrated|compute|hybrid|nvidia|query|power|switchable|reset|cache-create|cache-delete|cache-query)
                has_subcmd="$i" ;;
            md5|sha1|sha256|sha512) has_hash_algo=1 ;;
        esac
    done

    # 如果已经有顶层参数被使用了，不再补全顶层参数
    if [[ -n $has_S || -n $has_U || -n $has_g || -n $has_H || -n $has_h || -n $has_V ]]; then
        case "$prev" in
            -S)
                # 补全内核文件路径
                _filedir
                return
                ;;
            -H)
                # 补全哈希算法
                COMPREPLY=($(compgen -W "md5 sha1 sha256 sha512" -- "$cur"))
                return
                ;;
            -g)
                # 补全显卡操作
                COMPREPLY=($(compgen -W "integrated compute hybrid nvidia query power switchable reset cache-create cache-delete cache-query" -- "$cur"))
                return
                ;;
        esac

        if [[ -n $has_H && -n $has_hash_algo ]]; then
            # -H 算法之后补全文件路径或 --string/-s 标志
            if [[ "$cur" == -* ]]; then
                COMPREPLY=($(compgen -W "--string -s" -- "$cur"))
            else
                _filedir
            fi
            return
        fi

        if [[ -n $has_g && -n $has_subcmd ]]; then
            # 检测前一个参数是否是显卡子命令，如果是则补全高级选项
            case "$prev" in
                --coolbits|--rtd3)
                    # 数值参数，不做补全
                    return
                    ;;
                --use-nvidia-current|--force-comp)
                    # 布尔 flag，不需要额外参数
                    return
                    ;;
                *)
                    # 检查当前子命令是否需要高级选项补全
                    local subcmd="${words[has_subcmd]}"
                    if [[ "$subcmd" =~ ^(integrated|compute|hybrid|nvidia)$ ]]; then
                        if [[ "$cur" == -* ]]; then
                            COMPREPLY=($(compgen -W "--coolbits --rtd3 --use-nvidia-current --force-comp" -- "$cur"))
                            return
                        fi
                    elif [[ "$subcmd" == "power" ]]; then
                        COMPREPLY=($(compgen -W "on off auto" -- "$cur"))
                        return
                    fi
                    ;;
            esac
        fi

        # 有顶层参数但不在上述分支，无更多可补全项
        return
    fi

    # 还没有任何顶层参数，补全顶层参数
    if [[ "$cur" == -* ]]; then
        # 以短横线开头
        COMPREPLY=($(compgen -W "-S -U -g -H -h -V --help --version" -- "$cur"))
    else
        COMPREPLY=($(compgen -W "-S -U -g -H -h -V --help --version" -- "$cur"))
    fi
}

# 将补全函数注册到 ftool
complete -F _ftool ftool
