import SwiftUI
import UIKit

/// The card heading a tab or a pane's capsule, as the desktop draws it: a
/// coloured circle carrying the mark of what the pane runs, in the card's
/// glyph colour (`Core.tabIcon`). A busy pane's mark gives way to the
/// spinner, which shows alone where there is no card (tab icons off, or
/// a card from before the host's came).
struct TabIconCard: View {
    let core: Core
    var id: String?
    var size: CGFloat
    var busy = false
    /// The cards' revision (`TerminalModel.iconsRevision`): a new one
    /// draws the card again.
    var revision: UInt64 = 0
    @Environment(\.displayScale) private var scale

    var body: some View {
        if let id, let icon = TabIconImages.shared.icon(id, points: size, scale: scale, core: core) {
            ZStack {
                Circle().fill(icon.circle)
                if busy {
                    Spinner(color: icon.glyph).padding(size * 0.22)
                } else if let mask = icon.mask {
                    Image(uiImage: mask)
                        .renderingMode(.template)
                        .resizable()
                        .foregroundColor(icon.glyph)
                        .frame(width: size * 0.56, height: size * 0.56)
                }
            }
            .frame(width: size, height: size)
            .accessibilityHidden(true)
        } else if busy {
            Spinner(color: .white.opacity(0.7))
                .frame(width: size * 0.7, height: size * 0.7)
                .frame(width: size, height: size)
                .accessibilityHidden(true)
        }
    }
}

/// The cards drawn so far, by id and pixel size: each is drawn once and
/// kept while the cards stay the same -- a few dozen built-in ones and at
/// most 64 of a host's own, at the one or two sizes the app uses.
final class TabIconImages {
    static let shared = TabIconImages()

    struct Icon {
        let circle: Color
        let glyph: Color
        /// The glyph's shape, white at its coverage; nil for a card without.
        let mask: UIImage?
    }

    private var cache: [String: Icon] = [:]
    /// Ids the cards have none for, so as not to ask again.
    private var missing: Set<String> = []
    /// The core's revision of the cards in `cache`.
    private var revision: UInt64 = 0

    private init() {
        // Drawn again when next shown: let the memory go when it is short.
        NotificationCenter.default.addObserver(
            forName: UIApplication.didReceiveMemoryWarningNotification, object: nil, queue: .main
        ) { [weak self] _ in
            self?.cache.removeAll()
            self?.missing.removeAll()
        }
    }

    func icon(_ id: String, points: CGFloat, scale: CGFloat, core: Core) -> Icon? {
        let now = core.tabIconsRevision()
        if now != revision {
            // Another host's cards, or the built-in ones again.
            cache.removeAll()
            missing.removeAll()
            revision = now
        }
        let pixels = UInt32((points * 0.56 * scale).rounded(.up))
        let key = "\(id)@\(pixels)"
        if let icon = cache[key] { return icon }
        if missing.contains(key) { return nil }
        guard let card = core.tabIcon(id: id, size: pixels) else {
            missing.insert(key)
            return nil
        }
        let icon = Icon(
            circle: Color(hex: card.circle) ?? .gray,
            glyph: Color(hex: card.glyph) ?? .white,
            mask: Self.image(card, scale: scale)
        )
        cache[key] = icon
        return icon
    }

    /// The core's coverage bytes as a white image with that alpha, which a
    /// template image is tinted through.
    private static func image(_ card: TabIcon, scale: CGFloat) -> UIImage? {
        let size = Int(card.size)
        guard size > 0, card.mask.count == size * size else { return nil }
        var rgba = [UInt8](repeating: 0, count: size * size * 4)
        for (i, alpha) in card.mask.enumerated() {
            // Premultiplied white: every channel at the coverage.
            rgba[i * 4] = alpha
            rgba[i * 4 + 1] = alpha
            rgba[i * 4 + 2] = alpha
            rgba[i * 4 + 3] = alpha
        }
        guard let provider = CGDataProvider(data: Data(rgba) as CFData),
              let image = CGImage(
                width: size, height: size, bitsPerComponent: 8, bitsPerPixel: 32, bytesPerRow: size * 4,
                space: CGColorSpaceCreateDeviceRGB(),
                bitmapInfo: CGBitmapInfo(rawValue: CGImageAlphaInfo.premultipliedLast.rawValue),
                provider: provider, decode: nil, shouldInterpolate: true, intent: .defaultIntent
              )
        else { return nil }
        return UIImage(cgImage: image, scale: scale, orientation: .up).withRenderingMode(.alwaysTemplate)
    }
}
