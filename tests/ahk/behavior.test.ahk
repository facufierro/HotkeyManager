#Requires AutoHotkey v2.0
#SingleInstance Off
#NoTrayIcon
#Warn All, StdOut

global syntheticDown := Map(), chordHolds := Map(), heldOutputCounts := Map(), mirroredMouseDown := Map()
global repeatDown := Map(), repeatChord := Map(), enabled := Map()
global behaviorClipboardBackup := "", behaviorClipboardPending := false, behaviorClipboardSequence := 0
global testEvents := [], testOnSend := 0, testReleaseTime := 0
OnError(TestUnhandledError)

RunTest("release precedes trailing delay and press", TestSequence)
RunTest("release during prefix skips the later hold", TestEarlyRelease)
RunTest("releasing a chord member resumes the sequence", TestChordRelease)
RunTest("another owner retains its held output", TestSharedOutput)
RunTest("standalone combined holds still release together", TestStandaloneHold)
RunTest("failed hold send releases owned input", TestFailedSend)
RunTest("partial hold failure preserves another owner", TestPartialFailure)
FileAppend("All 7 behavior tests passed.`n", "*")
ExitApp 0

RunTest(name, test) {
    global testEvents, testOnSend, testReleaseTime
    testEvents := []
    testOnSend := 0
    testReleaseTime := 0
    test.Call()
    Assert(!syntheticDown.Count && !chordHolds.Count && !heldOutputCounts.Count, name " left input owned")
    Assert(!A_IsCritical, name " left the thread critical")
    FileAppend("PASS: " name "`n", "*")
}

TestSequence() {
    global testOnSend
    testOnSend := ScheduleSequenceRelease
    HoldBehaviorDown("test", "press(x);sleep(150);hold(XButton2);sleep(150);press(x)", "NumpadEnter")
    AssertKeys(["{blind}{x down}", "{blind}{x up}", "{blind}{xbutton2 downr}",
        "{blind}{xbutton2 up}", "{blind}{x down}", "{blind}{x up}"])
    Assert(testEvents[3]["time"] - testEvents[2]["time"] >= 150, "prefix delay was skipped")
    Assert(testEvents[5]["time"] - testReleaseTime >= 150, "trailing delay started before release")
}

ScheduleSequenceRelease(keys) {
    if InStr(keys, "{xbutton2 downr}")
        SetTimer ReleaseSequence, -60
}

ReleaseSequence(*) {
    global testReleaseTime
    Assert(testEvents.Length = 3, "trailing steps ran while the trigger was held")
    HoldBehaviorDown("test", "press(z);hold(XButton2)", "NumpadEnter")
    Assert(testEvents.Length = 3, "auto-repeat replayed the prefix")
    testReleaseTime := A_TickCount
    ReleaseTriggerHolds("NumpadEnter")
}

TestEarlyRelease() {
    global testOnSend
    testOnSend := ScheduleEarlyRelease
    HoldBehaviorDown("test", "press(x);sleep(150);hold(XButton2);sleep(150);press(x)", "NumpadEnter")
    AssertKeys(["{blind}{x down}", "{blind}{x up}", "{blind}{x down}", "{blind}{x up}"])
}

ScheduleEarlyRelease(keys) {
    if (keys = "{blind}{x down}" && chordHolds.Has("test"))
        SetTimer () => ReleaseTriggerHolds("NumpadEnter"), -1
}

TestChordRelease() {
    global testOnSend
    testOnSend := ScheduleChordRelease
    HoldBehaviorDown("test", "sleep(1);hold(XButton2);press(z)", "x NumpadEnter")
    AssertKeys(["{blind}{xbutton2 downr}", "{blind}{xbutton2 up}", "{blind}{z down}", "{blind}{z up}"])
}

ScheduleChordRelease(keys) {
    if InStr(keys, "{xbutton2 downr}")
        SetTimer () => ReleaseTriggerHolds("x"), -60
}

TestSharedOutput() {
    HoldChordDown("other", "XButton2", "F1")
    SetTimer ReleaseSharedOutput, -60
    HoldBehaviorDown("test", "sleep(1);hold(XButton2);press(z)", "NumpadEnter")
    Assert(heldOutputCounts["xbutton2"] = 1, "sequence released another owner's hold")
    AssertKeys(["{blind}{xbutton2 downr}", "{blind}{z down}", "{blind}{z up}"])
    HoldChordUp("other")
    Assert(testEvents[4]["keys"] = "{blind}{xbutton2 up}", "last owner did not release the mouse")
}

ReleaseSharedOutput(*) {
    Assert(heldOutputCounts["xbutton2"] = 2, "shared hold was not acquired")
    ReleaseTriggerHolds("NumpadEnter")
}

TestStandaloneHold() {
    HoldChordDown("test", "XButton2 RButton", "NumpadEnter")
    Assert(testEvents.Length = 2, "standalone hold did not press both buttons")
    ReleaseTriggerHolds("NumpadEnter")
    AssertKeys(["{blind}{xbutton2 downr}", "{blind}{rbutton downr}", "{blind}{rbutton up}{xbutton2 up}"])
}

TestFailedSend() {
    global testOnSend
    testOnSend := FailMouseDown
    failed := false
    try HoldBehaviorDown("test", "sleep(1);hold(XButton2);press(x)", "NumpadEnter")
    catch Error {
        failed := true
    }
    Assert(failed, "send failure was swallowed")
    AssertKeys(["{blind}{xbutton2 downr}", "{blind}{xbutton2 up}"])
}

TestPartialFailure() {
    global testOnSend
    HoldChordDown("other", "RButton", "F1")
    testOnSend := FailMouseDown
    failed := false
    try HoldBehaviorDown("test", "sleep(1);hold(XButton2 RButton);press(x)", "NumpadEnter")
    catch Error {
        failed := true
    }
    Assert(failed, "partial send failure was swallowed")
    Assert(heldOutputCounts["rbutton"] = 1, "failure released a key this sequence never acquired")
    AssertKeys(["{blind}{rbutton downr}", "{blind}{xbutton2 downr}", "{blind}{xbutton2 up}"])
    HoldChordUp("other")
}

FailMouseDown(keys) {
    if InStr(keys, "{xbutton2 downr}")
        throw Error("simulated send failure")
}

TestSend(keys) {
    global testEvents
    keys := StrLower(keys)
    testEvents.Push(Map("keys", keys, "time", A_TickCount))
    if IsObject(testOnSend)
        testOnSend.Call(keys)
}

AssertKeys(expected) {
    Assert(testEvents.Length = expected.Length, "unexpected input event count")
    for index, keys in expected
        Assert(testEvents[index]["keys"] = keys, "unexpected input event at index " index)
}

Assert(condition, message) {
    if !condition
        throw Error(message)
}

TestUnhandledError(failure, *) {
    FileAppend("FAIL: " failure.Message " at line " failure.Line "`n", "*")
    ExitApp 1
}

SendOverlayCommand(*) {
    throw Error("unexpected backend command")
}
SendAppEvent(*) {
    throw Error("unexpected backend event")
}
UriEncode(*) {
    throw Error("unexpected URI encoding")
}
CheckRepeatReleases(*) {
    throw Error("unexpected repeat timer")
}
