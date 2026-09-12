package app.waker.android;

import com.google.androidgamesdk.GameActivity;

/**
 * Android host for the Rust Waker application.
 *
 * GameActivity supplies GameTextInput and the Android lifecycle bridge;
 * application UI and rendering remain in Rust.
 */
public final class WakerActivity extends GameActivity {
}