# Render only datad startup entries; preserve all unrelated commands.
function trim(s) { sub(/^[ \t]+/, "", s); sub(/[ \t\r]+$/, "", s); return s }
function datad_path(s) {
    return s ~ /^\/(data\/(plugins\/|data\/)?(zwrt-datad|u60-datad)\/(service\.sh|zwrt-datad|u60-datad)|etc\/init\.d\/(zwrt-datad|u60-datad))$/
}
function plain(s,    a,n,i,p,check) {
    s=trim(s)
    sub(/[ \t]+#.*/, "", s)
    gsub(/["\047]/, "", s)
    n=split(s,a,/[ \t]+/); i=1
    while (i<=n && (a[i]=="nohup" || a[i]=="sh" || a[i]=="/bin/sh" || a[i]=="/system/bin/sh" || a[i]=="exec")) i++
    p=a[i]
    if (!datad_path(p)) return 0
    if (p ~ /service\.sh$/ || p ~ /^\/etc\/init\.d\//) {
        if (a[i+1]!="start" && a[i+1]!="restart" && a[i+1]!="enable") return 0
    }
    # Never remove a second, unrelated command on the same logical line.
    check=s
    gsub(/[012]?>&[012]/, "", check)
    if (check ~ /[;|]/ || check ~ /&&/ || check ~ /&[ \t]*[^ \t]/) return 0
    return 1
}
function startup(s,    t) {
    if (plain(s)) return 1
    t=trim(s)
    # Common one-line file/executable guard around a single start command.
    if (t ~ /^(\[|test)[ \t]+-(f|x)[ \t]/ && t ~ /&&/) {
        sub(/^.*&&[ \t]*/, "", t)
        return plain(t)
    }
    return 0
}
function guarded(s,    t,a,n) {
    t=trim(s); gsub(/["\047]/,"",t)
    if (t !~ /^if[ \t]+\[[ \t]+-(f|x)[ \t]+.*[ \t]+\][ \t]*;[ \t]*then$/) return 0
    n=split(t,a,/[ \t]+/)
    return datad_path(a[4])
}
function flush_line(raw,logical,    t) {
    t=trim(logical)
    if (t ~ /^# (BEGIN|END) (zwrt-datad|u60-datad) managed startup$/) return
    if (startup(t)) {
        removed++
        if (!first_removed) first_removed=count+1
        return
    }
    if (t !~ /^#/ && t ~ /\/(zwrt-datad|u60-datad)(\/service\.sh|[ \t"\047]|$)/ && t !~ /zwrt-datad-cloud/) {
        print "Unsupported mixed datad startup command; rc.local left unchanged" > "/dev/stderr"
        bad=1
    }
    count++; lines[count]=raw
    if (t ~ /^(fi|done|esac)([ \t;]|$)/ && depth>0) depth--
    if (!boundary && depth==0 && t ~ /^exit([ \t]|$)/) boundary=count
    if (!boundary && (t ~ /\/ufi-tools\/service\.sh[ \t]+start/ || t ~ /\/ufitools[^ \t]*\/service\.sh[ \t]+start/)) boundary=depth>0 ? outer_start : count
    if (t ~ /^(if|for|while|until|case)[ \t]/ && t !~ /;[ \t]*(fi|done|esac)([ \t;]|$)/) {
        if (depth==0) outer_start=count
        depth++
    }
}
{
    if (raw!="") raw=raw "\n"
    raw=raw $0
    part=$0
    if (part ~ /\\[ \t]*$/) {
        sub(/\\[ \t]*$/, "", part); logical=logical part " "; next
    }
    logical=logical part
    record_count++; record_raw[record_count]=raw; record_text[record_count]=logical
    raw=""; logical=""
}
END {
    if (raw!="") { print "Incomplete shell continuation" > "/dev/stderr"; bad=1 }
    for (record=1;record<=record_count;record++) {
        if (guarded(record_text[record]) && startup(record_text[record+1]) && trim(record_text[record+2])=="fi") {
            flush_line(record_raw[record+1],record_text[record+1]); record+=2
        } else flush_line(record_raw[record],record_text[record])
    }
    if (bad) exit 2
    insert=boundary ? boundary : count+1
    if (first_removed && first_removed<insert) insert=first_removed
    for (i=1;i<=count+1;i++) {
        if (i==insert) print "sh /data/zwrt-datad/service.sh start"
        if (i<=count) print lines[i]
    }
}
