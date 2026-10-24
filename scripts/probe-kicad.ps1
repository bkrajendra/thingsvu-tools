# SPDX-License-Identifier: GPL-3.0-or-later
#
# Phase 0 KiCad IPC probe (Windows). Speaks NNG REQ0 over the NNG ipc transport (a named pipe)
# with hand-encoded protobuf envelopes, so it needs no NNG library, protoc, or Python.
# Results are recorded in docs/kicad-api-compatibility.md. Not production code.
#
# Scenarios:
#   basic      GetVersion, Ping, GetOpenDocuments, an unknown command
#   schematic  open documents, hierarchy, netlist; -Place also places Device:R in memory
#   demo       two-resistor series chain: place, set values, wire, labels, netlist read-back;
#              -SaveCopyPath also saves a copy
#   save       SaveCopyOfDocument of the loaded schematic to -SaveCopyPath
#
# Example (headless):
#   kicad-cli api-server --socket "$env:TEMP\tv\k.sock" path\to\project.kicad_sch
#   .\scripts\probe-kicad.ps1 -PipePath "$env:TEMP\tv\k.sock" -Scenario demo -SaveCopyPath C:\out\copy.kicad_sch
#
# Run against disposable projects only: the demo scenario modifies the loaded schematic.
param(
    [string]$PipePath = (Join-Path $env:TEMP 'kicad\api.sock'),   # KiCad's default socket path
    [int]$TimeoutMs = 5000,
    [ValidateSet('basic', 'schematic', 'demo', 'save')][string]$Scenario = 'basic',
    [string]$SaveCopyPath = '',         # demo scenario: SaveCopyOfDocument target (empty = skip)
    [switch]$Place                      # schematic scenario: also place Device:R in memory (no save)
)
$ErrorActionPreference = 'Stop'

# ---------- protobuf helpers ----------
function Varint([UInt64]$v) {
    $b = New-Object System.Collections.Generic.List[byte]
    do { $x = [byte]($v -band 0x7F); $v = $v -shr 7; if ($v -ne 0) { $x = $x -bor 0x80 }; $b.Add($x) } while ($v -ne 0)
    ,$b.ToArray()
}
function FieldBytes([int]$num, [byte[]]$data) { ,([byte[]]((Varint (($num -shl 3) -bor 2)) + (Varint $data.Length) + $data)) }
function FieldStr([int]$num, [string]$s) { FieldBytes $num ([Text.Encoding]::UTF8.GetBytes($s)) }
function FieldVarint([int]$num, [UInt64]$v) { ,([byte[]]((Varint ($num -shl 3)) + (Varint $v))) }

function ApiRequest([string]$typeName, [byte[]]$inner) {
    $header = FieldStr 2 "thingsvu-probe-$PID"                     # client_name; kicad_token left empty
    $any = [byte[]]((FieldStr 1 "type.googleapis.com/$typeName") + (FieldBytes 2 $inner))
    ,([byte[]]((FieldBytes 1 $header) + (FieldBytes 2 $any)))
}

function ReadVarint([byte[]]$b, [ref]$i) {
    [UInt64]$r = 0; $s = 0
    while ($true) { $x = $b[$i.Value]; $i.Value++; $r = $r -bor ([UInt64]($x -band 0x7F) -shl $s); if (($x -band 0x80) -eq 0) { break }; $s += 7 }
    $r
}
# Generic decoder: prints field tree; length-delimited fields shown as string if printable, else recursed.
function Dump([byte[]]$b, [int]$depth = 0) {
    $i = 0; $pad = '  ' * $depth
    while ($i -lt $b.Length) {
        $key = ReadVarint $b ([ref]$i); $num = $key -shr 3; $wt = $key -band 7
        switch ($wt) {
            0 { "$pad#$num = $(ReadVarint $b ([ref]$i))" }
            2 {
                $len = [int](ReadVarint $b ([ref]$i)); $sub = [byte[]]$b[$i..($i + $len - 1)]; if ($len -eq 0) { $sub = @() }; $i += $len
                $txt = [Text.Encoding]::UTF8.GetString($sub)
                if ($len -gt 0 -and $txt -match '^[\x20-\x7E]+$') { "$pad#$num = `"$txt`"" }
                else { "$pad#$num {"; try { Dump $sub ($depth + 1) } catch { "$pad  <bytes:$len>" }; "$pad}" }
            }
            1 { $i += 8; "$pad#$num = <fixed64>" }
            5 { $i += 4; "$pad#$num = <fixed32>" }
            default { throw "bad wire type $wt" }
        }
    }
}

# ---------- NNG ipc transport ----------
$pipe = New-Object System.IO.Pipes.NamedPipeClientStream('.', $PipePath, [System.IO.Pipes.PipeDirection]::InOut)
$pipe.Connect($TimeoutMs)
"connected to \\.\pipe\$PipePath"

function ReadExact([int]$n) {
    $buf = New-Object byte[] $n; $off = 0
    while ($off -lt $n) {
        $t = $pipe.ReadAsync($buf, $off, $n - $off)
        if (-not $t.Wait($TimeoutMs)) { throw "read timeout" }
        if ($t.Result -le 0) { throw "pipe closed" }
        $off += $t.Result
    }
    ,$buf
}

# SP header: 00 'S' 'P' 00 <proto u16 BE> 00 00 ; REQ0 = 0x30, peer REP0 = 0x31
$pipe.Write([byte[]](0, 0x53, 0x50, 0, 0, 0x30, 0, 0), 0, 8); $pipe.Flush()
$peer = ReadExact 8
"handshake reply: $(($peer | % { $_.ToString('X2') }) -join ' ')"
if ($peer[1] -ne 0x53 -or $peer[5] -ne 0x31) { throw "unexpected peer header" }

$script:reqId = [UInt32]2147483648   # NNG request ids carry the high bit
function Call([string]$typeName, [byte[]]$inner, [switch]$Quiet) {
    $script:reqId++
    $idBytes = [BitConverter]::GetBytes([UInt32]$script:reqId); [Array]::Reverse($idBytes)
    $body = [byte[]]($idBytes + (ApiRequest $typeName $inner))
    $lenBytes = [BitConverter]::GetBytes([UInt64]$body.Length); [Array]::Reverse($lenBytes)
    $frame = [byte[]](,[byte]1 + $lenBytes + $body)
    $pipe.Write($frame, 0, $frame.Length); $pipe.Flush()
    $hdr = ReadExact 9
    $lb = [byte[]]$hdr[1..8]; [Array]::Reverse($lb); $len = [BitConverter]::ToUInt64($lb, 0)
    $resp = ReadExact ([int]$len)
    $payload = [byte[]]$resp[4..($resp.Length - 1)]                # strip echoed request id
    if (-not $Quiet) {
        Write-Host "`n=== $typeName  (request id 0x$($script:reqId.ToString('X8')), $len bytes)"
        Dump $payload | Write-Host
    }
    ,$payload
}

# Return every length-delimited occurrence of field $num in message $b
function GetFields([byte[]]$b, [int]$num) {
    $out = New-Object System.Collections.Generic.List[object]; $i = 0
    while ($i -lt $b.Length) {
        $key = ReadVarint $b ([ref]$i); $n = $key -shr 3; $wt = $key -band 7
        switch ($wt) {
            0 { [void](ReadVarint $b ([ref]$i)) }
            2 { $len = [int](ReadVarint $b ([ref]$i)); $sub = if ($len) { [byte[]]$b[$i..($i + $len - 1)] } else { [byte[]]@() }; $i += $len; if ($n -eq $num) { $out.Add($sub) } }
            1 { $i += 8 } 5 { $i += 4 }
        }
    }
    ,$out.ToArray()
}
function AnyValue([byte[]]$resp) { (GetFields (GetFields $resp 3)[0] 2)[0] }   # ApiResponse.message(Any).value

if ($Scenario -eq 'basic') {
    [void](Call 'kiapi.common.commands.GetVersion' @())
    [void](Call 'kiapi.common.commands.Ping' @())
    [void](Call 'kiapi.common.commands.GetOpenDocuments' (FieldVarint 1 1))
    [void](Call 'kiapi.common.commands.ThisCommandDoesNotExist' @())
}
if ($Scenario -eq 'schematic') {
    $r = Call 'kiapi.common.commands.GetOpenDocuments' (FieldVarint 1 1)       # DOCTYPE_SCHEMATIC
    $docs = GetFields (AnyValue $r) 1
    if ($docs.Count -eq 0) { throw "no schematic document open" }
    $doc = [byte[]]$docs[0]                                                     # DocumentSpecifier, reused verbatim
    [void](Call 'kiapi.common.commands.GetOpenDocuments' (FieldVarint 1 3))   # DOCTYPE_PCB
    [void](Call 'kiapi.schematic.commands.GetSchematicHierarchy' (FieldBytes 1 $doc))
    [void](Call 'kiapi.schematic.commands.GetSchematicNetlist' (FieldBytes 1 $doc))
    if ($Place) {
        # Headless mode does not preload libraries: start the async load, then poll Device's status.
        [void](Call 'kiapi.common.commands.LoadAllLibraries' (FieldVarint 1 1))   # LT_SYMBOL
        $statusReq = [byte[]]((FieldVarint 1 1) + (FieldVarint 2 3))             # LT_SYMBOL, LTS_BOTH
        $sw = [Diagnostics.Stopwatch]::StartNew(); $deviceStatus = $null; $libCount = 0
        while ($sw.Elapsed.TotalSeconds -lt 120) {
            $r = Call 'kiapi.common.commands.GetLibraryStatuses' $statusReq -Quiet
            $entries = GetFields (AnyValue $r) 1; $libCount = $entries.Count
            foreach ($e in $entries) {
                $tableEntry = [byte[]](GetFields $e 1)[0]                       # LibraryStatusEntry.entry
                $nick = [Text.Encoding]::UTF8.GetString([byte[]](GetFields $tableEntry 1)[0])   # .nickname
                if ($nick -eq 'Device') { $i = 0; $deviceStatus = $null
                    while ($i -lt $e.Length) { $k = ReadVarint $e ([ref]$i); if (($k -band 7) -eq 2) { $l = [int](ReadVarint $e ([ref]$i)); $i += $l } else { $v = ReadVarint $e ([ref]$i); if (($k -shr 3) -eq 2) { $deviceStatus = $v } } } }
            }
            if ($deviceStatus -eq 3 -or $deviceStatus -eq 4 -or $deviceStatus -eq 1) { break }
            Start-Sleep -Milliseconds 500
        }
        Write-Host "`n--- symbol libraries in table: $libCount; 'Device' status: $deviceStatus (3=LOADED) after $([int]$sw.Elapsed.TotalSeconds)s"
        $hdr = FieldBytes 1 $doc
        $lib = [byte[]]((FieldStr 1 'Device') + (FieldStr 2 'R'))
        $pos = [byte[]]((FieldVarint 1 25400000) + (FieldVarint 2 25400000))   # 25.4 mm, 25.4 mm (on 1.27 mm grid)
        $req = [byte[]]((FieldBytes 1 $hdr) + (FieldBytes 2 $lib) + (FieldBytes 3 $pos) + (FieldStr 6 'R1'))
        [void](Call 'kiapi.schematic.commands.PlaceSymbolFromLibrary' $req)
        [void](Call 'kiapi.schematic.commands.GetSchematicNetlist' (FieldBytes 1 $doc))
    }
}

if ($Scenario -eq 'demo') {
    function Mm([double]$mm) { [UInt64][Math]::Round($mm * 1000000) }
    function Vec([double]$x, [double]$y) { ,([byte[]]((FieldVarint 1 (Mm $x)) + (FieldVarint 2 (Mm $y)))) }
    function AnyOf([string]$type, [byte[]]$value) { ,([byte[]]((FieldStr 1 "type.googleapis.com/$type") + (FieldBytes 2 $value))) }
    function Status([byte[]]$resp) { $i = 0; $s = (GetFields $resp 2)[0]; $k = ReadVarint $s ([ref]$i); ReadVarint $s ([ref]$i) }
    # Rebuild $msg with length-delimited field $num replaced by $newValue (first occurrence)
    function ReplaceField([byte[]]$msg, [int]$num, [byte[]]$newValue) {
        $out = New-Object System.Collections.Generic.List[byte]; $i = 0; $done = $false
        while ($i -lt $msg.Length) {
            $start = $i; $key = ReadVarint $msg ([ref]$i); $n = $key -shr 3; $wt = $key -band 7
            switch ($wt) { 0 { [void](ReadVarint $msg ([ref]$i)) } 2 { $l = [int](ReadVarint $msg ([ref]$i)); $i += $l } 1 { $i += 8 } 5 { $i += 4 } }
            if ($wt -eq 2 -and $n -eq $num -and -not $done) { $out.AddRange([byte[]](FieldBytes $num $newValue)); $done = $true }
            else { $out.AddRange([byte[]]$msg[$start..($i - 1)]) }
        }
        ,$out.ToArray()
    }

    $r = Call 'kiapi.common.commands.GetOpenDocuments' (FieldVarint 1 1) -Quiet
    $doc = [byte[]](GetFields (AnyValue $r) 1)[0]
    $hdr = FieldBytes 1 $doc                                                    # ItemHeader{document}
    [void](Call 'kiapi.common.commands.LoadAllLibraries' (FieldVarint 1 1) -Quiet)

    function PlaceR([string]$ref, [double]$x, [double]$y) {
        $lib = [byte[]]((FieldStr 1 'Device') + (FieldStr 2 'R'))
        $req = [byte[]]((FieldBytes 1 $hdr) + (FieldBytes 2 $lib) + (FieldBytes 3 (Vec $x $y)) + (FieldStr 6 $ref))
        $sw = [Diagnostics.Stopwatch]::StartNew()
        while ($true) {
            $resp = Call 'kiapi.schematic.commands.PlaceSymbolFromLibrary' $req -Quiet
            if ((Status $resp) -eq 1) { break }
            if ($sw.Elapsed.TotalSeconds -gt 90) { Dump $resp | Write-Host; throw "placement of $ref failed" }
            Start-Sleep -Milliseconds 500
        }
        $inst = [byte[]](GetFields (AnyValue $resp) 2)[0] | % { $_ }             # PlaceFromLibraryResponse.item (Any)
        $inst = [byte[]](GetFields ([byte[]]$inst) 2)[0]                          # Any.value = SchematicSymbolInstance
        $kiid = [Text.Encoding]::UTF8.GetString([byte[]](GetFields ([byte[]](GetFields $inst 1)[0]) 1)[0])
        Write-Host "placed $ref at ($x, $y) mm after $([int]$sw.Elapsed.TotalSeconds)s -> KIID $kiid"
        ,$inst
    }
    # Device:R pins sit at local y = -3.81 (pin 1) and +3.81 (pin 2); all coordinates are on the 1.27 mm grid.
    $r1 = PlaceR 'R1' 50.8 50.8     # pin1 (50.8, 46.99)  pin2 (50.8, 54.61)
    $r2 = PlaceR 'R2' 50.8 63.5     # pin1 (50.8, 59.69)  pin2 (50.8, 67.31)

    $c = Call 'kiapi.common.commands.BeginCommit' $hdr
    $commitId = [byte[]](GetFields (AnyValue $c) 1)[0]                          # KIID message

    function WithValue([byte[]]$inst, [string]$value) {
        $vf = [byte[]](GetFields $inst 8)[0]                                     # value_field (SchematicField)
        $text = [byte[]](GetFields $vf 3)[0]                                     # SchematicField.text (Text)
        $vf = ReplaceField $vf 3 (ReplaceField $text 5 ([Text.Encoding]::UTF8.GetBytes($value)))
        ,(ReplaceField $inst 8 $vf)
    }
    $upd = [byte[]]((FieldBytes 1 $hdr) +
        (FieldBytes 2 (AnyOf 'kiapi.schematic.types.SchematicSymbolInstance' (WithValue $r1 '10k'))) +
        (FieldBytes 2 (AnyOf 'kiapi.schematic.types.SchematicSymbolInstance' (WithValue $r2 '10k'))))
    [void](Call 'kiapi.common.commands.UpdateItems' $upd)

    function Wire([double]$x1, [double]$y1, [double]$x2, [double]$y2) {
        AnyOf 'kiapi.schematic.types.SchematicLine' ([byte[]]((FieldBytes 2 (Vec $x1 $y1)) + (FieldBytes 3 (Vec $x2 $y2)) + (FieldVarint 4 1)))
    }
    function Label([string]$name, [double]$x, [double]$y) {
        $text = [byte[]]((FieldBytes 2 (Vec $x $y)) + (FieldStr 5 $name))
        AnyOf 'kiapi.schematic.types.LocalLabel' ([byte[]]((FieldBytes 2 (Vec $x $y)) + (FieldBytes 3 $text)))
    }
    $create = [byte[]]((FieldBytes 1 $hdr) +
        (FieldBytes 2 (Wire 50.8 54.61 50.8 59.69)) +        # R1.2 -> R2.1
        (FieldBytes 2 (Label 'INPUT' 50.8 46.99)) +          # on R1.1
        (FieldBytes 2 (Label 'MID' 50.8 57.15)) +            # on the wire
        (FieldBytes 2 (Label 'OUTPUT' 50.8 67.31)))          # on R2.2
    [void](Call 'kiapi.common.commands.CreateItems' $create)

    $end = [byte[]]((FieldBytes 1 $commitId) + (FieldVarint 2 1) + (FieldStr 3 'thingsvu probe: two-resistor demo') + (FieldBytes 4 $hdr))
    [void](Call 'kiapi.common.commands.EndCommit' $end)

    [void](Call 'kiapi.schematic.commands.GetSchematicNetlist' (FieldBytes 1 $doc))

    if ($SaveCopyPath) {
        $opts = [byte[]]((FieldVarint 1 0) + (FieldVarint 2 1))                   # overwrite=false, include_project=true
        [void](Call 'kiapi.common.commands.SaveCopyOfDocument' ([byte[]]((FieldBytes 1 $doc) + (FieldStr 2 $SaveCopyPath) + (FieldBytes 3 $opts))))
    }
}

if ($Scenario -eq 'save') {
    $r = Call 'kiapi.common.commands.GetOpenDocuments' (FieldVarint 1 1) -Quiet
    $doc = [byte[]](GetFields (AnyValue $r) 1)[0]
    $opts = [byte[]]((FieldVarint 1 0) + (FieldVarint 2 1))                       # overwrite=false, include_project=true
    [void](Call 'kiapi.common.commands.SaveCopyOfDocument' ([byte[]]((FieldBytes 1 $doc) + (FieldStr 2 $SaveCopyPath) + (FieldBytes 3 $opts))))
}

$pipe.Dispose()
"`nclosed cleanly"
