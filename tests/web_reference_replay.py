#!/usr/bin/env python3
"""Compile the unmodified reference decoders for an offline JVM/JS differential audit.

Usage: python tests/web_reference_replay.py REFERENCE_ROOT CAPTURE... --output local/reference-replay.ndjson
Only the Android Context/engine host is substituted. No modem/device access.
Kotlin/compiler and JSON runtime jars are resolved from the existing Gradle cache.
"""
import argparse
from pathlib import Path
import os
import subprocess
import textwrap

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('reference', type=Path)
    parser.add_argument('captures', nargs='+', type=Path)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    source = args.reference / 'source/app/src/main/java/com/minikano/ufitools/client/data/zwrt/signal'
    work = ROOT / 'local/web-reference-jvm'
    work.mkdir(parents=True, exist_ok=True)
    cache = Path(os.environ.get('GRADLE_USER_HOME', Path.home() / '.gradle')) / 'caches/modules-2/files-2.1'

    def jar(group, name, version):
        found = list((cache / group / name / version).rglob(f'{name}-{version}.jar'))
        if len(found) != 1:
            raise RuntimeError(f'Missing cached runtime: {name} {version}')
        return str(found[0])

    kotlin = [jar('org.jetbrains.kotlin', n, '2.3.21') for n in ['kotlin-compiler-embeddable', 'kotlin-stdlib', 'kotlin-reflect', 'kotlin-script-runtime']]
    deps = [jar('org.jetbrains.intellij.deps', 'trove4j', '1.0.20200330'), jar('org.jetbrains.kotlinx', 'kotlinx-coroutines-core-jvm', '1.9.0')]
    annotations = next(p for p in cache.glob('org.jetbrains/annotations/*/*/annotations-*.jar') if not p.name.endswith(('-sources.jar','-javadoc.jar')))
    runtime = [kotlin[1], jar('org.jetbrains.kotlinx', 'kotlinx-serialization-json-jvm', '1.6.3'), jar('org.jetbrains.kotlinx', 'kotlinx-serialization-core-jvm', '1.6.3'), str(annotations)]
    context = work / 'Context.kt'
    context.write_text('package android.content\nclass Context { val assets=Assets() }\nclass Assets { fun open(path:String):java.io.InputStream=throw UnsupportedOperationException(path) }\n', encoding='utf-8')
    engine = source.joinpath('SignalCaptureEngine.kt').read_text(encoding='utf-8')
    cards = engine[engine.index('    data class Cards('):engine.index('    data class UiState(')]
    host = work / 'EngineHost.kt'
    host.write_text('package com.minikano.ufitools.client.data.zwrt.signal\nclass SignalCaptureEngine {\n' + cards + '\n}\n', encoding='utf-8')
    runner = work / 'ReferenceReplay.kt'
    runner.write_text(textwrap.dedent('''
        package com.minikano.ufitools.client.data.zwrt.signal
        import kotlinx.serialization.json.*
        import java.io.*
        import java.lang.reflect.Modifier
        fun value(v:Any?):JsonElement=when(v) {
            null -> JsonNull
            is JsonElement -> v
            is String -> JsonPrimitive(v)
            is Number -> JsonPrimitive(v)
            is Boolean -> JsonPrimitive(v)
            is Enum<*> -> JsonPrimitive(v.name)
            is Map<*,*> -> JsonObject(v.entries.associate { it.key.toString() to value(it.value) })
            is Iterable<*> -> JsonArray(v.map(::value))
            is Pair<*,*> -> JsonArray(listOf(value(v.first),value(v.second)))
            else -> JsonObject(v.javaClass.declaredFields.filterNot { Modifier.isStatic(it.modifiers)||it.isSynthetic }.associate { field -> field.isAccessible=true; field.name to value(field.get(v)) })
        }
        fun main(args:Array<String>) {
            val out=File(args[0]).bufferedWriter()
            for ((capture,path) in args.drop(1).withIndex()) {
                var cards=SignalCaptureEngine.Cards();var index=0
                var priorEngineering:Any?=null
                DataInputStream(File(path).inputStream().buffered()).use { input ->
                    while(true) {
                        val length=try { Integer.reverseBytes(input.readInt()) }catch(e:EOFException){break}
                        require(length in 12..65535)
                        val packet=ByteArray(length);input.readFully(packet)
                        val code=(packet[2].toInt() and 255) or ((packet[3].toInt() and 255) shl 8)
                        val event=SignalDecoder.decodePacket(code,packet,1_000_000L+index)
                        cards=SignalCardUpdates.apply(cards,code,packet,event)
                        val b=packet.copyOfRange(12,packet.size)
                        val parsed:Any?=when(code) {
                            0xB881->Ml1Decoder.b881UlTb(b);0xB888->Ml1Decoder.b888Pdsch(b)
                            0xB8A1->Ml1Decoder.b8a1Arbitration(b);0xB884->Ml1Decoder.b884PowerControl(b)
                            0xB8D1->Ml1Decoder.b8d1TxRf(b);0xB883->Ml1Decoder.b883UlSchedule(b)
                            0xB887->Ml1Decoder.b887PdschStatus(b);0xB890->Ml1Decoder.b890Cdrx(b)
                            0xB8DD->Ml1Decoder.b8ddServingFtl(b);0xB823->Ml1Decoder.b823ServingInfo(b)
                            0xB111,0xB114,0xB11B,0xB11D,0xB123,0xB12A,0xB134,0xB193,0xB1DC->Ml1Decoder.lteLog(code,b)
                            0xB0C1->LteSignalDecoder.mib(b);0xB0C2->LteSignalDecoder.serving(b)
                            0xB16E,0xB16F->LteSignalDecoder.power(code,b);0xB173->LteSignalDecoder.pdsch(b)
                            else->null
                        }
                        val compact=JsonObject((value(cards) as JsonObject).filterKeys { it !in setOf("nrUplinkPower","lteMeasurements","engineering","measurementSnapshot","neighbors","cellParams") })
                        val encoded=buildJsonObject {put("capture",capture);put("index",index);put("code",code);put("event",SignalEventJson.encode(event));put("parsed",value(parsed));put("cards",compact);put("data",value(cards.bearerQos.data));if(cards.engineering!=priorEngineering)put("engineering",value(cards.engineering))}
                        priorEngineering=cards.engineering
                        out.write(encoded.toString());out.newLine();index++
                    }
                }
            }
            out.close()
        }
    '''), encoding='utf-8')
    # Use the App's remaining onPacket card hooks verbatim (UL summary, B823,
    # NCI). These live outside SignalCardUpdates and must be audited as well.
    hooks = engine[engine.index('        // 卡片专用提取'):engine.index('        if (cards != _state.value.cards)')]
    program = runner.read_text(encoding='utf-8').replace(
        'val b=packet.copyOfRange(12,packet.size)',
        'val b=packet.copyOfRange(12,packet.size)\nval ev=event\nval u8Hook:ByteArray?=b\n' + hooks)
    runner.write_text(program, encoding='utf-8')
    names = ['SignalDecoder', 'SignalCardUpdates', 'SignalEventJson', 'Ml1Decoder', 'LteSignalDecoder', 'NasDecoder', 'DataBearerQos', 'RrcUperDecoder', 'RrcSchemaData', 'VendorFingerprint', 'RrcPlmn', 'CellMeasurementSnapshot', 'CellEngineering', 'LteMeasurementContext', 'NrLinkConfig', 'NrUplinkPower', 'AutomaticTimingAdvance', 'TimingAdvanceDistance', 'TemplateDecoder', 'BitReader']
    files = [str(source / (n + '.kt')) for n in names] + [str(context), str(host), str(runner)]
    target = work / 'reference.jar'
    subprocess.run(['java', '-Xmx2g', '-cp', os.pathsep.join(kotlin + deps + [str(annotations)]), 'org.jetbrains.kotlin.cli.jvm.K2JVMCompiler', '-no-stdlib', '-no-reflect', '-classpath', os.pathsep.join(runtime), '-jvm-target', '17', '-d', str(target), *files], check=True)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    subprocess.run(['java', '-Xmx2g', '-cp', os.pathsep.join([str(target), *runtime]), 'com.minikano.ufitools.client.data.zwrt.signal.ReferenceReplayKt', str(args.output), *(str(p) for p in args.captures)], check=True)
    print('Reference replay written. Source files were not modified; captured contents remain local.')


if __name__ == '__main__':
    main()
