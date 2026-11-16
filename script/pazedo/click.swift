// Posts a real left click at a screen point (in points), a right click with `--right`, or only
// moves the pointer with `--move`. System Events' `click at` goes through
// the accessibility tree, which gpui windows do not expose, so it never reaches Zed.
import CoreGraphics
import Foundation

let scroll = CommandLine.arguments.firstIndex(of: "--scroll").flatMap {
    CommandLine.arguments.indices.contains($0 + 1) ? Int32(CommandLine.arguments[$0 + 1]) : nil
}
let rightClick = CommandLine.arguments.contains("--right")
var arguments = CommandLine.arguments.filter { $0 != "--move" && $0 != "--right" }
if let index = arguments.firstIndex(of: "--scroll") {
    arguments.removeSubrange(index...min(index + 1, arguments.count - 1))
}
let moveOnly = CommandLine.arguments.contains("--move") || scroll != nil
guard arguments.count > 2, let x = Double(arguments[1]), let y = Double(arguments[2]) else {
    FileHandle.standardError.write("usage: click.swift [--move|--right] <x> <y>\n".data(using: .utf8)!)
    exit(2)
}
let point = CGPoint(x: x, y: y)
let source = CGEventSource(stateID: .hidSystemState)
if let scroll {
    CGEvent(mouseEventSource: source, mouseType: .mouseMoved, mouseCursorPosition: point, mouseButton: .left)?
        .post(tap: .cghidEventTap)
    usleep(60_000)
    CGEvent(scrollWheelEvent2Source: source, units: .line, wheelCount: 1, wheel1: scroll, wheel2: 0, wheel3: 0)?
        .post(tap: .cghidEventTap)
    exit(0)
}
let types: [CGEventType] =
    moveOnly ? [.mouseMoved]
    : rightClick ? [.mouseMoved, .rightMouseDown, .rightMouseUp]
    : [.mouseMoved, .leftMouseDown, .leftMouseUp]
for type in types {
    let event = CGEvent(
        mouseEventSource: source, mouseType: type, mouseCursorPosition: point,
        mouseButton: rightClick ? .right : .left)
    // Without it apps see a click count of 0 rather than a single click.
    event?.setIntegerValueField(.mouseEventClickState, value: 1)
    event?.post(tap: .cghidEventTap)
    usleep(60_000)
}
