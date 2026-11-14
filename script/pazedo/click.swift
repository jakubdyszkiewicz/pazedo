// Posts a real left click at a screen point (in points). System Events' `click at` goes through
// the accessibility tree, which gpui windows do not expose, so it never reaches Zed.
import CoreGraphics
import Foundation

guard CommandLine.arguments.count > 2,
    let x = Double(CommandLine.arguments[1]), let y = Double(CommandLine.arguments[2])
else {
    FileHandle.standardError.write("usage: click.swift <x> <y>\n".data(using: .utf8)!)
    exit(2)
}
let point = CGPoint(x: x, y: y)
let source = CGEventSource(stateID: .hidSystemState)
for type in [CGEventType.mouseMoved, .leftMouseDown, .leftMouseUp] {
    let event = CGEvent(mouseEventSource: source, mouseType: type, mouseCursorPosition: point, mouseButton: .left)
    event?.post(tap: .cghidEventTap)
    usleep(60_000)
}
