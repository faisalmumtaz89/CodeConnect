import SwiftUI

/// A table cell's width cap, shared by the iPhone's and the Mac's agent tables.
///
/// The cap as a real layout, because `frame(maxWidth:)` cannot do this
/// job: it passes the grid's unspecified measurement proposal through
/// untouched, reports the capped width anyway, and the row then clips
/// every line the measurement never counted — a 500-character URL cell
/// measured 20pt tall and drew ~300 (caught by the layout tests). This
/// clamps the *proposal*, so a capped cell's reported height is its
/// honest wrapped height and nothing truncates.
public struct WidthCap: Layout {
    public let cap: CGFloat

    public init(cap: CGFloat) { self.cap = cap }

    public func sizeThatFits(
        proposal: ProposedViewSize, subviews: Subviews, cache: inout ()
    ) -> CGSize {
        guard let cell = subviews.first else { return .zero }
        switch proposal.width {
        case .some(let width) where width > 0:
            // Height stays unconstrained on purpose: forwarding the
            // grid's height probe let a (240 × one-line) proposal report
            // one line, and the URL cell rendered `…` — caught on a
            // rendered fixture, forbidden by the no-truncation rule.
            return cell.sizeThatFits(
                ProposedViewSize(width: min(width, cap), height: nil))
        case .some:
            // The zero-width minimum probe is not ours to shape.
            return cell.sizeThatFits(proposal)
        case .none:
            let ideal = cell.sizeThatFits(.unspecified)
            if ideal.width <= cap { return ideal }
            return cell.sizeThatFits(ProposedViewSize(width: cap, height: nil))
        }
    }

    public func placeSubviews(
        in bounds: CGRect, proposal: ProposedViewSize, subviews: Subviews,
        cache: inout ()
    ) {
        // Height-free again: the bounds came from the honest measurement
        // above, and a concrete height here is an invitation to ellipsise
        // if the two ever disagree by a rounding error.
        subviews.first?.place(
            at: bounds.origin, anchor: .topLeading,
            proposal: ProposedViewSize(width: bounds.width, height: nil))
    }
}
