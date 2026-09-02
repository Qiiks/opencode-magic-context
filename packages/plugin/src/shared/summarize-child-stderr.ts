/**
 * Return the useful part of a child-process diagnostic without allowing a
 * bundled source line to hide the error that follows it.
 *
 * Node prints uncaught exceptions as source context, a caret, and then the
 * error line and stack. Prefer that error line plus the first two stack frames;
 * when no recognizable error line exists, the end of stderr is most likely to
 * contain the actionable failure detail.
 */
const MAX_CHILD_STDERR_SUMMARY_LENGTH = 500;
const CHILD_ERROR_LINE_PATTERN = /^(?:[A-Za-z]*Error|SqliteError|Error)\b[^\n]*/;
const STACK_FRAME_PATTERN = /^\s*at\b/;

export function summarizeChildStderr(stderr: string): string {
    const lines = stderr.split(/\r?\n/);
    const errorLineIndex = lines.findIndex((line) => CHILD_ERROR_LINE_PATTERN.test(line));

    if (errorLineIndex < 0) {
        return stderr.length > MAX_CHILD_STDERR_SUMMARY_LENGTH
            ? stderr.slice(-MAX_CHILD_STDERR_SUMMARY_LENGTH)
            : stderr;
    }

    const excerptLines = [lines[errorLineIndex]];
    let stackFrames = 0;
    for (let index = errorLineIndex + 1; index < lines.length && stackFrames < 2; index += 1) {
        if (!STACK_FRAME_PATTERN.test(lines[index])) continue;
        excerptLines.push(lines[index]);
        stackFrames += 1;
    }

    return excerptLines.join("\n").slice(0, MAX_CHILD_STDERR_SUMMARY_LENGTH);
}
