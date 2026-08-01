import AVFoundation
import SwiftUI
import UIKit
import VisionKit

/// Camera scanner for the `codeconnect pair` QR code.
///
/// `DataScannerViewController` only, with no AVFoundation fallback, and that is
/// a considered choice rather than a gap: it requires an A12 or newer chip,
/// which is *exactly* the set of devices that can run iOS 17 — this app's
/// deployment target. Every device that can install CodeConnect can run this
/// scanner, so a second code path would exist only to be untested. Where it is
/// genuinely unavailable — the Simulator, or camera access denied — the caller
/// shows the manual path instead, which never goes away.
///
/// **Apple's overlay is switched off.** `isGuidanceEnabled` and
/// `isHighlightingEnabled` bring a floating system label and a yellow highlight
/// that belong to a different design language; `CCScannerFrame` draws ours.
struct QRScannerView: UIViewControllerRepresentable {
    /// Called with the raw payload string, on the main actor.
    var onScan: (String) -> Void
    /// Called when the scanner itself fails, with something worth reading.
    var onError: (String) -> Void

    /// Whether this device can scan at all. Checked before presenting so the
    /// user is never shown a camera that cannot work.
    static var isSupported: Bool { DataScannerViewController.isSupported }

    /// Whether scanning is permitted right now. False when camera access has
    /// been denied or restricted.
    static var isAvailable: Bool { DataScannerViewController.isAvailable }

    static var cameraAuthorization: AVAuthorizationStatus {
        AVCaptureDevice.authorizationStatus(for: .video)
    }

    // MARK: Torch

    private static var torchDevice: AVCaptureDevice? {
        guard let device = AVCaptureDevice.default(for: .video), device.hasTorch else { return nil }
        return device
    }

    /// Whether a torch control is worth drawing at all.
    static var hasTorch: Bool { torchDevice?.isTorchAvailable == true }

    /// Sets the torch and reports what it actually ended up as — the caller must
    /// not assume a control worked just because it was tapped.
    @discardableResult
    static func setTorch(_ on: Bool) -> Bool {
        guard let device = torchDevice, device.isTorchAvailable else { return false }
        do {
            try device.lockForConfiguration()
            defer { device.unlockForConfiguration() }
            device.torchMode = on ? .on : .off
            return device.torchMode == .on
        } catch {
            return false
        }
    }

    func makeUIViewController(context: Context) -> DataScannerViewController {
        let scanner = DataScannerViewController(
            recognizedDataTypes: [.barcode(symbologies: [.qr, .microQR])],
            qualityLevel: .balanced,
            recognizesMultipleItems: false,
            isHighFrameRateTrackingEnabled: false,
            isPinchToZoomEnabled: true,
            // Ours, not Apple's — `CCScannerFrame` draws the guidance.
            isGuidanceEnabled: false,
            isHighlightingEnabled: false)
        scanner.delegate = context.coordinator
        // Force-dark chrome: the preview is a UIKit surface, and anything the
        // system draws over it — the pinch-to-zoom affordance, an alert — takes
        // its appearance from the trait, not from SwiftUI.
        scanner.overrideUserInterfaceStyle = .dark
        scanner.view.backgroundColor = UIColor(CC.color.bg)
        return scanner
    }

    func updateUIViewController(_ scanner: DataScannerViewController, context: Context) {
        context.coordinator.start(scanner)
    }

    static func dismantleUIViewController(
        _ scanner: DataScannerViewController, coordinator: Coordinator
    ) {
        scanner.stopScanning()
        // The torch belongs to the device, not to this view: leaving it on
        // after the scanner closes would be a light nobody asked for.
        setTorch(false)
    }

    func makeCoordinator() -> Coordinator {
        Coordinator(onScan: onScan, onError: onError)
    }

    @MainActor
    final class Coordinator: NSObject, DataScannerViewControllerDelegate {
        private let onScan: (String) -> Void
        private let onError: (String) -> Void
        private var scanning = false
        /// One payload per presentation. A QR stays in frame for many
        /// milliseconds, and a pairing code is single-use — offering it twice
        /// would spend it and then report the second attempt as a failure.
        private var delivered = false

        init(onScan: @escaping (String) -> Void, onError: @escaping (String) -> Void) {
            self.onScan = onScan
            self.onError = onError
        }

        func start(_ scanner: DataScannerViewController) {
            guard !scanning else { return }
            scanning = true
            do {
                try scanner.startScanning()
            } catch {
                scanning = false
                onError(error.localizedDescription)
            }
        }

        func dataScanner(
            _ dataScanner: DataScannerViewController, didAdd addedItems: [RecognizedItem],
            allItems: [RecognizedItem]
        ) {
            deliver(from: addedItems)
        }

        func dataScanner(
            _ dataScanner: DataScannerViewController, didUpdate updatedItems: [RecognizedItem],
            allItems: [RecognizedItem]
        ) {
            deliver(from: updatedItems)
        }

        func dataScanner(
            _ dataScanner: DataScannerViewController, becameUnavailableWithError error: DataScannerViewController.ScanningUnavailable
        ) {
            scanning = false
            onError(String(describing: error))
        }

        private func deliver(from items: [RecognizedItem]) {
            guard !delivered else { return }
            for item in items {
                guard case .barcode(let barcode) = item, let payload = barcode.payloadStringValue
                else { continue }
                delivered = true
                // The haptic and the 400ms hold belong to the presenting view,
                // which owns the "CODE READ" beat — firing one here too would
                // put two haptics inside 400ms, which reads as a glitch.
                onScan(payload)
                return
            }
        }
    }
}
