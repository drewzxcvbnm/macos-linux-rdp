import Foundation
import IOKit.pwr_mgt

/// Keeps the display awake (and wakes it) while a client is connected.
public final class AwakeAssertion {
    private var assertionID: IOPMAssertionID = 0
    private var activityID: IOPMAssertionID = 0

    public init() {
        IOPMAssertionCreateWithName(kIOPMAssertionTypePreventUserIdleDisplaySleep as CFString,
                                    IOPMAssertionLevel(kIOPMAssertionLevelOn),
                                    "macrd remote session" as CFString, &assertionID)
        IOPMAssertionDeclareUserActivity("macrd remote session" as CFString, kIOPMUserActiveLocal, &activityID)
    }

    deinit {
        if assertionID != 0 { IOPMAssertionRelease(assertionID) }
        if activityID != 0 { IOPMAssertionRelease(activityID) }
    }
}
