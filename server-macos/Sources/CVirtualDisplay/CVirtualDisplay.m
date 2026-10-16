#import "CVirtualDisplay.h"

BOOL MacRDVirtualDisplayAvailable(void) {
    return NSClassFromString(@"CGVirtualDisplay") != nil;
}
